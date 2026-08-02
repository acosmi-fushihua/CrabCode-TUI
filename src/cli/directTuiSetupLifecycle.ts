import type { ChannelEntry } from '../bootstrap/state.js'
import {
  getAllowedChannels,
  setAllowedChannels,
  setHasDevChannels,
} from '../bootstrap/state.js'
import {
  CRABCODE_TUI_SETUP_PROTOCOL_VERSION,
  CRABCODE_TUI_SETUP_SUBTYPE,
  CrabCodeTuiApiKeyApprovalResponseSchema,
  CrabCodeTuiAutoModeOptInResponseSchema,
  CrabCodeTuiBypassPermissionsConsentResponseSchema,
  CrabCodeTuiDevelopmentChannelsResponseSchema,
  CrabCodeTuiExternalCrabcodeMdResponseSchema,
  CrabCodeTuiGroveTermsResponseSchema,
  CrabCodeTuiMcpServerApprovalResponseSchema,
  CrabCodeTuiOnboardingCustomApiKeyResponseSchema,
  CrabCodeTuiOnboardingCustomBaseUrlResponseSchema,
  CrabCodeTuiOnboardingCustomModelIdResponseSchema,
  CrabCodeTuiOnboardingCustomProviderResponseSchema,
  CrabCodeTuiOnboardingLanguageResponseSchema,
  CrabCodeTuiOnboardingOAuthBrowserResponseSchema,
  CrabCodeTuiOnboardingOAuthBrowserOpenFailedResponseSchema,
  CrabCodeTuiOnboardingOAuthErrorResponseSchema,
  CrabCodeTuiOnboardingOAuthPlatformResponseSchema,
  CrabCodeTuiOnboardingOAuthSelectResponseSchema,
  CrabCodeTuiOnboardingOAuthSuccessResponseSchema,
  CrabCodeTuiOnboardingPreflightResponseSchema,
  CrabCodeTuiOnboardingSecurityResponseSchema,
  CrabCodeTuiOnboardingTerminalResponseSchema,
  CrabCodeTuiOnboardingThemeResponseSchema,
  type CrabCodeTuiSetupRequest,
} from './crabcodeTuiBridgeProtocol.js'
import type { NativeTuiRendererSession } from '../entrypoints/nativeTuiRendererSession.js'
import { docsUrl, setLocale, t } from '../i18n/index.js'
import { checkGate_CACHED_OR_BLOCKING } from '../services/analytics/growthbook.js'
import { logEvent } from '../services/analytics/index.js'
import {
  getAuthStatus,
  loginStream,
  resolveOAuthCompletionScopes,
} from '../services/acosmi/client.js'
import { installOAuthTokens } from '../services/auth/installOAuthTokens.js'
import {
  runDirectTuiGroveTermsBarrier,
  type DirectTuiGroveRenderer,
} from '../services/api/groveDirectTui.js'
import { getMcpConfigsByScope } from '../services/mcp/config.js'
import { isChannelsEnabled } from '../services/mcp/channelAllowlist.js'
import { getProjectMcpServerStatus } from '../services/mcp/utils.js'
import {
  getAcosmiOAuthTokens,
  hasAcosmiApiKeyAuth,
  isAcosmiAuthEnabled,
  isAcosmiSubscriber,
  saveApiKey,
  validateForceLoginOrg,
} from '../utils/auth.js'
import { normalizeApiKeyForConfig } from '../utils/authPortable.js'
import {
  getCustomApiKeyStatus,
  getGlobalConfig,
  saveCurrentProjectConfig,
  saveGlobalConfig,
} from '../utils/config.js'
import {
  getExternalCrabcodeMdIncludes,
  getMemoryFiles,
  shouldShowCrabcodeMdExternalIncludesWarning,
} from '../utils/crabcodemd.js'
import { env } from '../utils/env.js'
import {
  isEnvDefinedFalsy,
  isEnvTruthy,
  isRunningOnHomespace,
} from '../utils/envUtils.js'
import { feature } from '../utils/featurePolyfill.js'
import { gracefulShutdownSync } from '../utils/gracefulShutdown.js'
import { executeNotificationHooks } from '../utils/hooks.js'
import { checkOnboardingEndpoints } from '../utils/preflightChecksCore.js'
import { getSettingsWithAllErrors } from '../utils/settings/allErrors.js'
import {
  getInitialSettings,
  hasAutoModeOptIn,
  hasSkipDangerousModePermissionPrompt,
  updateSettingsForSource,
} from '../utils/settings/settings.js'
import { getSystemThemeName } from '../utils/systemTheme.js'
import {
  type ThemeName,
  type ThemeSetting,
} from '../utils/theme.js'

const REQUEST_BASE = {
  subtype: CRABCODE_TUI_SETUP_SUBTYPE,
  protocol_version: CRABCODE_TUI_SETUP_PROTOCOL_VERSION,
} as const

const ONBOARDING_THEME_SETTINGS: readonly ThemeSetting[] = [
  'auto',
  'dark',
  'light',
  'dark-daltonized',
  'light-daltonized',
  'dark-ansi',
  'light-ansi',
]

export type DirectTuiSetupRequester = NativeTuiRendererSession['requestSetup']

export type DirectTuiSetupLifecycleOptions = {
  permissionMode: string
  allowDangerouslySkipPermissions: boolean
  devChannels: ChannelEntry[]
}

export type DirectTuiPreTrustSetupResult = {
  onboardingShown: boolean
}

type SetupExit = (exitCode: number) => never

export type DirectTuiCustomModelSetupInput = {
  provider: 'openai-compatible'
  baseUrl: string
  modelId: string
  apiKey: string
}

export type DirectTuiSetupAuthorities = {
  shouldSkipSetup(): boolean
  hasUsableAuthentication(): Promise<boolean>
  exit: SetupExit
  runGrove(
    renderer: DirectTuiGroveRenderer,
    location: 'onboarding' | 'policy_update_modal',
  ): Promise<boolean>
  installTerminal(theme: ThemeName): Promise<void>
  configureCustomModel(input: DirectTuiCustomModelSetupInput): Promise<void>
}

function defaultAuthorities(): DirectTuiSetupAuthorities {
  return {
    shouldSkipSetup: () =>
      process.env.NODE_ENV === 'test' || isEnvTruthy(process.env.IS_DEMO),
    async hasUsableAuthentication(): Promise<boolean> {
      // Preserve the historical non-Acosmi/provider boundary: API-key
      // helpers, explicit third-party auth and --bare do not require the
      // Acosmi OAuth onboarding flow. For the standard Acosmi route, reuse
      // the exact authorities that the former REPL verification hook used.
      if (!isAcosmiAuthEnabled()) return true
      if (hasAcosmiApiKeyAuth()) return true
      let sdkAuthorized = false
      try {
        sdkAuthorized = (await getAuthStatus())?.authorized === true
      } catch {
        // A storage/client read failure is not proof of authentication.
      }
      // OAuth completion is a dual persistence transaction: the SDK token
      // store and CrabCode secure storage must both have survived. Either
      // half alone reproduces the unusable post-onboarding state.
      return sdkAuthorized && isAcosmiSubscriber()
    },
    exit(exitCode): never {
      gracefulShutdownSync(exitCode)
      throw new Error(
        `graceful shutdown unexpectedly returned for exit ${exitCode}`,
      )
    },
    runGrove: runDirectTuiGroveTermsBarrier,
    async installTerminal(theme): Promise<void> {
      // The terminal setup authority predates the renderer split. It is loaded
      // only after the user explicitly selects install; no React tree is
      // created and the existing backend aggregate remains authoritative.
      const { setupTerminal } = await import(
        '../commands/terminalSetup/terminalSetup.js'
      )
      await setupTerminal(theme)
    },
    async configureCustomModel(input): Promise<void> {
      const apiKey = input.apiKey.trim()
      const baseUrl = input.baseUrl.trim()
      const modelId = input.modelId.trim()
      await saveApiKey(apiKey)
      const modelAlias = modelId.replace(/[^a-zA-Z0-9_-]/g, '-')
      // Exact product values from the fixed historical direct-TUI source:
      // 2358212c2df2018816058c8a03b1ac3d324e74e0,
      // src/components/ConsoleOAuthFlow.tsx:664-680. These are preserved
      // product behavior, not renderer-side capability inference.
      const settingsResult = updateSettingsForSource('userSettings', {
        customModel: {
          provider: 'openai-compatible',
          baseUrl,
          apiKey,
          models: {
            [modelAlias]: {
              id: modelId,
              displayName: modelId,
              contextWindow: 128_000,
              maxOutputTokens: 16_384,
              supportsTools: true,
              isDefault: true,
            },
          },
        },
      } as any)
      if (settingsResult.error) throw settingsResult.error
    },
  }
}

function authoritiesWithOverrides(
  overrides: Partial<DirectTuiSetupAuthorities> | undefined,
): DirectTuiSetupAuthorities {
  return { ...defaultAuthorities(), ...overrides }
}

/**
 * Historical pre-trust setup segment.
 *
 * This runs after config/auth initialization has made the TypeScript
 * authorities available and setup() has selected its final cwd, but before
 * the historical workspace-trust dialog point. It does not discover or
 * connect project MCP.
 */
export async function runDirectTuiPreTrustOnboarding(
  requestSetup: DirectTuiSetupRequester,
  authorityOverrides?: Partial<DirectTuiSetupAuthorities>,
): Promise<DirectTuiPreTrustSetupResult> {
  const authorities = authoritiesWithOverrides(authorityOverrides)
  if (authorities.shouldSkipSetup()) return { onboardingShown: false }

  const config = getGlobalConfig()
  if (
    config.theme &&
    config.hasCompletedOnboarding &&
    (await authorities.hasUsableAuthentication())
  ) {
    return { onboardingShown: false }
  }

  const oauthEnabled = isAcosmiAuthEnabled()
  logEvent('tengu_began_setup', { oauthEnabled })

  const language = await requestSetup(
    {
      ...REQUEST_BASE,
      kind: 'onboarding',
      stage: 'language',
      title: safeTitle('选择界面语言 / Select interface language'),
      body: [safeLine(t('onboarding_lang_select_hint'))],
      options: [
        { value: 'zh-CN', label: '中文（简体）' },
        { value: 'en-US', label: 'English' },
      ],
    },
    CrabCodeTuiOnboardingLanguageResponseSchema,
  )
  if (language.decision === 'select') {
    saveGlobalConfig(current => ({
      ...current,
      uiLanguage: language.locale,
    }))
    setLocale(language.locale)
  }

  if (oauthEnabled) {
    logOnboardingStep(oauthEnabled, 'preflight')
    const result = await checkOnboardingEndpoints()
    if (!result.success) {
      await requestSetup(
        {
          ...REQUEST_BASE,
          kind: 'onboarding',
          stage: 'preflight',
          title: safeTitle(t('native_tui_preflight_failed_title')),
          body: [safeLine(t('native_tui_connectivity_failed_fallback'))],
          ...(result.error ? { error: safeLine(result.error) } : {}),
          ...(result.sslHint ? { ssl_hint: safeLine(result.sslHint) } : {}),
        },
        CrabCodeTuiOnboardingPreflightResponseSchema,
      )
      process.stderr.write(
        `[preflight] ${safeLine(result.error ?? 'connectivity check failed')}\n`,
      )
      await delay(100)
    }
  }

  logOnboardingStep(oauthEnabled, 'theme')
  const availableThemes = ONBOARDING_THEME_SETTINGS.filter(
    setting => setting !== 'auto' || feature('AUTO_THEME'),
  )
  const syntaxHighlightingDisabled =
    getInitialSettings().syntaxHighlightingDisabled ?? false
  const syntaxToggleEnabled = !isEnvDefinedFalsy(
    process.env.CRABCODE_SYNTAX_HIGHLIGHT,
  )
  const theme = await requestSetup(
    {
      ...REQUEST_BASE,
      kind: 'onboarding',
      stage: 'theme',
      title: safeTitle(t('native_tui_theme_title')),
      body: [safeLine(t('onboarding_theme_change_hint'))],
      options: availableThemes.map(value => ({
        value,
        label: themeLabel(value),
      })),
      syntax_toggle_enabled: syntaxToggleEnabled,
    },
    CrabCodeTuiOnboardingThemeResponseSchema,
  )
  if (!availableThemes.includes(theme.theme)) {
    throw new Error('Native TUI selected a theme that was not offered')
  }
  if (
    !syntaxToggleEnabled &&
    theme.syntax_highlighting_disabled !== syntaxHighlightingDisabled
  ) {
    throw new Error(
      'Native TUI changed syntax highlighting while the environment disables the toggle',
    )
  }
  const selectedThemeSetting = theme.theme
  saveGlobalConfig(current => ({
    ...current,
    theme: selectedThemeSetting,
  }))
  if (theme.syntax_highlighting_disabled !== syntaxHighlightingDisabled) {
    // Fixed historical ThemePicker behavior: the settings authority owns
    // persistence error reporting; onboarding still advances.
    updateSettingsForSource('userSettings', {
      syntaxHighlightingDisabled: theme.syntax_highlighting_disabled,
    })
  }

  let skipOAuth = false
  const onboardingKey = customApiKeyNeedingApproval()
  if (onboardingKey) {
    logOnboardingStep(oauthEnabled, 'api-key')
    const approval = await requestSetup(
      {
        ...REQUEST_BASE,
        kind: 'api_key_approval',
        title: safeTitle(t('native_tui_use_api_key_title')),
        body: [
          safeLine(t('approve_api_key_question')),
          safeLine(`ACOSMI_API_KEY: …${onboardingKey}`),
        ],
      },
      CrabCodeTuiApiKeyApprovalResponseSchema,
    )
    persistApiKeyDecision(onboardingKey, approval.decision === 'accept')
    skipOAuth = approval.decision === 'accept'
  }

  if (oauthEnabled && !skipOAuth) {
    logOnboardingStep(oauthEnabled, 'oauth')
    await runOnboardingOAuth(requestSetup, authorities.configureCustomModel)
  }

  logOnboardingStep(oauthEnabled, 'security')
  await requestSetup(
    {
      ...REQUEST_BASE,
      kind: 'onboarding',
      stage: 'security',
      title: safeTitle(t('onboarding_security_notes')),
      body: [
        safeLine(t('onboarding_crabcode_mistakes')),
        safeLine(t('onboarding_review_responses')),
        safeLine(t('onboarding_prompt_injection_risk')),
        safeLine(t('onboarding_for_more_details')),
      ],
      security_url: docsUrl('security'),
    },
    CrabCodeTuiOnboardingSecurityResponseSchema,
  )
  if (shouldOfferTerminalSetupWithoutUi()) {
    logOnboardingStep(oauthEnabled, 'terminal-setup')
    const terminal = await requestSetup(
      {
        ...REQUEST_BASE,
        kind: 'onboarding',
        stage: 'terminal',
        title: safeTitle(t('onboarding_terminal_setup_question')),
        body: [
          safeLine(
            env.terminal === 'Apple_Terminal'
              ? t('native_tui_terminal_option_enter')
              : t('native_tui_terminal_shift_enter'),
          ),
        ],
        options: [
          {
            value: 'install',
            label: safeLabel(t('onboarding_terminal_setup_yes')),
          },
          {
            value: 'skip',
            label: safeLabel(t('onboarding_terminal_setup_no')),
          },
        ],
      },
      CrabCodeTuiOnboardingTerminalResponseSchema,
    )
    if (terminal.decision === 'install') {
      const resolvedTheme: ThemeName =
        selectedThemeSetting === 'auto'
          ? getSystemThemeName()
          : selectedThemeSetting
      // Historical behavior logs/swallow setup failures and still completes
      // onboarding. The terminal authority itself owns detailed error logging.
      await authorities.installTerminal(resolvedTheme).catch(() => {})
    }
  }

  saveGlobalConfig(current => ({
    ...current,
    hasCompletedOnboarding: true,
    lastOnboardingVersion: MACRO.VERSION,
  }))
  return { onboardingShown: true }
}

/**
 * Historical post-trust setup segment. The caller must invoke this only after
 * setup() and the single historical canonical-CWD trust screen have completed.
 */
export async function runDirectTuiPostTrustSetup(
  requestSetup: DirectTuiSetupRequester,
  options: DirectTuiSetupLifecycleOptions,
  onboardingShown: boolean,
  afterTrustSideEffects: () => void | Promise<void>,
  authorityOverrides?: Partial<DirectTuiSetupAuthorities>,
): Promise<void> {
  const authorities = authoritiesWithOverrides(authorityOverrides)
  if (!authorities.shouldSkipSetup()) {
    if (!isEnvTruthy(process.env.CLAUBBIT)) {
      await runProjectMcpApproval(requestSetup)
      await runExternalCrabcodeMdApproval(requestSetup)
    }
    await afterTrustSideEffects()

    const groveContinues = await authorities.runGrove(
      createGroveRenderer(requestSetup),
      onboardingShown ? 'onboarding' : 'policy_update_modal',
    )
    if (!groveContinues) {
      logEvent('tengu_grove_policy_exited', {})
      authorities.exit(0)
    }

    await runStandaloneApiKeyApproval(requestSetup)
    await runBypassConsent(requestSetup, options, authorities.exit)
    await runAutoModeConsent(requestSetup, options, authorities.exit)
    await runDevelopmentChannels(requestSetup, options, authorities.exit)
  }
}

async function runOnboardingOAuth(
  requestSetup: DirectTuiSetupRequester,
  configureCustomModel: DirectTuiSetupAuthorities['configureCustomModel'],
): Promise<void> {
  const settings = getInitialSettings()
  const forcedMethod = settings.forceLoginMethod
  if (
    forcedMethod != null &&
    forcedMethod !== 'acosmi' &&
    forcedMethod !== 'console'
  ) {
    throw new Error(
      'Direct TUI onboarding received an unsupported forced login method',
    )
  }
  if (forcedMethod === 'acosmi') {
    logEvent('tengu_oauth_acosmi_forced', {})
  } else if (forcedMethod === 'console') {
    logEvent('tengu_oauth_console_forced', {})
  }
  while (true) {
    const offeredMethods = oauthOptions(forcedMethod)
    const methodWasSelected = forcedMethod == null
    const method =
      forcedMethod === 'acosmi' || forcedMethod === 'console'
        ? forcedMethod
        : (
            await requestSetup(
              {
                ...REQUEST_BASE,
                kind: 'onboarding',
                stage: 'oauth',
                phase: 'select_method',
                title: safeTitle(t('native_tui_login_method_title')),
                body: [safeLine(t('native_tui_login_method_body'))],
                options: offeredMethods,
              },
              CrabCodeTuiOnboardingOAuthSelectResponseSchema,
            )
          ).method
    if (!offeredMethods.some(option => option.value === method)) {
      throw new Error(
        'Native TUI selected an authentication method that was not offered',
      )
    }
    if (methodWasSelected) {
      if (method === 'platform') {
        logEvent('tengu_oauth_platform_selected', {})
      } else if (method === 'console') {
        logEvent('tengu_oauth_console_selected', {})
      } else {
        logEvent('tengu_oauth_acosmi_selected', {})
      }
    }
    if (method === 'platform') {
      await requestSetup(
        {
          ...REQUEST_BASE,
          kind: 'onboarding',
          stage: 'oauth',
          phase: 'platform_setup',
          title: safeTitle(t('oauth_platform_setup_title')),
          body: [
            safeLine(t('oauth_platform_setup_instruction')),
            safeLine(t('oauth_platform_enterprise_contact')),
          ],
          links: [
            {
              label: safeLabel(t('oauth_platform_doc_china')),
              url: docsUrl('providers-china-region'),
            },
            {
              label: safeLabel(t('oauth_platform_doc_global')),
              url: docsUrl('providers-global-region'),
            },
            {
              label: safeLabel(t('oauth_platform_doc_routing')),
              url: docsUrl('model-routing'),
            },
          ],
        },
        CrabCodeTuiOnboardingOAuthPlatformResponseSchema,
      )
      continue
    }

    try {
      if (usesDirectCustomModelFlow(method, forcedMethod)) {
        const configured = await runDirectTuiCustomModelSetup(
          requestSetup,
          configureCustomModel,
        )
        if (!configured) continue
      } else {
        await completeDirectOAuth(requestSetup, method)
      }
      const success = await requestSetup(
        {
          ...REQUEST_BASE,
          kind: 'onboarding',
          stage: 'oauth',
          phase: 'success',
          title: safeTitle(t('oauth_success')),
          body: [safeLine(t('native_tui_auth_success_body'))],
        },
        CrabCodeTuiOnboardingOAuthSuccessResponseSchema,
      )
      if (success.decision === 'continue') {
        logEvent('tengu_oauth_success', {
          loginWithAcosmi: method === 'acosmi',
        })
        void import('../utils/model/modelCapabilities.js')
          .then(({ refreshModelCapabilities }) => refreshModelCapabilities())
          .catch(() => {})
        return
      }
    } catch (error) {
      if (!(error instanceof DirectOAuthEventError)) {
        logEvent('tengu_oauth_error', {
          error: errorMessage(error) as never,
          ssl_error: false,
        })
      }
      const message = safeLine(errorMessage(error))
      await requestSetup(
        {
          ...REQUEST_BASE,
          kind: 'onboarding',
          stage: 'oauth',
          phase: 'error',
          title: safeTitle(t('native_tui_auth_failed_title')),
          body: [message],
          error: message,
        },
        CrabCodeTuiOnboardingOAuthErrorResponseSchema,
      )
      await delay(1_000)
    }
  }
}

export function usesDirectCustomModelFlow(
  method: 'acosmi' | 'console' | 'platform',
  forcedMethod: unknown,
): boolean {
  return method === 'console' && forcedMethod !== 'console'
}

/**
 * Fixed historical direct-TUI custom endpoint flow.
 *
 * This is a renderer-neutral state machine. It deliberately exposes only the
 * four interactions present in the pinned CrabCode TUI: one provider choice,
 * base URL, model ID and a masked API key. `false` means the user backed out
 * to the login-method menu; it is not an authentication failure.
 */
export async function runDirectTuiCustomModelSetup(
  requestSetup: DirectTuiSetupRequester,
  configureCustomModel: DirectTuiSetupAuthorities['configureCustomModel'],
): Promise<boolean> {
  const provider = 'openai-compatible' as const
  let phase:
    | 'custom_provider'
    | 'custom_base_url'
    | 'custom_model_id'
    | 'custom_api_key' = 'custom_provider'
  let baseUrl = ''
  let modelId = ''
  let apiKeyError: string | undefined

  while (true) {
    if (phase === 'custom_provider') {
      const response = await requestSetup(
        {
          ...REQUEST_BASE,
          kind: 'onboarding',
          stage: 'oauth',
          phase,
          title: safeTitle(t('custom_provider_title')),
          body: [safeLine(t('apikey_input_back_hint'))],
          options: [
            {
              value: provider,
              label: safeLabel(t('custom_provider_openai')),
            },
          ],
        },
        CrabCodeTuiOnboardingCustomProviderResponseSchema,
      )
      if (response.decision === 'back') return false
      phase = 'custom_base_url'
      continue
    }

    if (phase === 'custom_base_url') {
      const response = await requestSetup(
        {
          ...REQUEST_BASE,
          kind: 'onboarding',
          stage: 'oauth',
          phase,
          title: safeTitle(t('custom_baseurl_title')),
          body: [
            safeLine(t('custom_baseurl_instruction')),
            safeLine(t('apikey_input_back_hint')),
          ],
          initial_value: baseUrl,
        },
        CrabCodeTuiOnboardingCustomBaseUrlResponseSchema,
      )
      if (response.decision === 'back') {
        phase = 'custom_provider'
        continue
      }
      baseUrl = response.base_url
      phase = 'custom_model_id'
      continue
    }

    if (phase === 'custom_model_id') {
      const response = await requestSetup(
        {
          ...REQUEST_BASE,
          kind: 'onboarding',
          stage: 'oauth',
          phase,
          title: safeTitle(t('custom_model_title')),
          body: [
            safeLine(t('custom_model_instruction')),
            safeLine(t('apikey_input_back_hint')),
          ],
          initial_value: modelId,
        },
        CrabCodeTuiOnboardingCustomModelIdResponseSchema,
      )
      if (response.decision === 'back') {
        phase = 'custom_base_url'
        continue
      }
      modelId = response.model_id
      apiKeyError = undefined
      phase = 'custom_api_key'
      continue
    }

    const response = await requestSetup(
      {
        ...REQUEST_BASE,
        kind: 'onboarding',
        stage: 'oauth',
        phase,
        title: safeTitle(t('apikey_input_title')),
        body: [
          safeLine(t('apikey_input_instruction')),
          safeLine(t('apikey_input_back_hint')),
        ],
        ...(apiKeyError ? { error: safeLine(apiKeyError) } : {}),
      },
      CrabCodeTuiOnboardingCustomApiKeyResponseSchema,
    )
    if (response.decision === 'back') {
      apiKeyError = undefined
      phase = 'custom_model_id'
      continue
    }
    try {
      await configureCustomModel({
        provider,
        baseUrl,
        modelId,
        apiKey: response.api_key,
      })
      logEvent('tengu_custom_model_configured', {
        provider: provider as never,
        model_id: modelId as never,
      })
      return true
    } catch (error) {
      // The plaintext key is intentionally neither retained nor interpolated
      // into the error request. A retry asks the renderer for a fresh secret.
      apiKeyError = errorMessage(error)
    }
  }
}

async function completeDirectOAuth(
  requestSetup: DirectTuiSetupRequester,
  method: 'acosmi' | 'console',
): Promise<void> {
  const controller = new AbortController()
  const settings = getInitialSettings()
  logEvent('tengu_oauth_flow_start', {
    loginWithAcosmi: method === 'acosmi',
  })
  const requestedScopes =
    method === 'acosmi' ? ['ai', 'skills', 'account'] : ['ai']
  let completed = false
  const rendererDeliveries: Promise<unknown>[] = []
  for await (const event of loginStream(
    requestedScopes,
    { orgUUID: settings.forceLoginOrgUUID },
    controller.signal,
  )) {
    if (event.type === 'auth_url') {
      if (!event.url) throw new Error('OAuth authority returned no login URL')
      rendererDeliveries.push(
        requestSetup(
          {
            ...REQUEST_BASE,
            kind: 'onboarding',
            stage: 'oauth',
            phase: 'browser_url',
            title: safeTitle(t('oauth_opening_browser')),
            body: [
              safeLine(t('oauth_browser_not_open')),
              safeLine(
                `(${t('shortcut_template', {
                  shortcut: 'c',
                  action: 'copy',
                })})`,
              ),
            ],
            url: event.url,
          },
          CrabCodeTuiOnboardingOAuthBrowserResponseSchema,
        ),
      )
      continue
    }
    if (event.type === 'error') {
      if (event.err_code === 'browser_open_failed') {
        // Fixed historical ConsoleOAuthFlow keeps this event non-fatal and
        // reveals the already-delivered manual URL immediately. This closed
        // process-private status changes renderer state only; it is not a new
        // backend event or SDK capability.
        rendererDeliveries.push(
          requestSetup(
            {
              ...REQUEST_BASE,
              kind: 'onboarding',
              stage: 'oauth',
              phase: 'browser_open_failed',
            },
            CrabCodeTuiOnboardingOAuthBrowserOpenFailedResponseSchema,
          ),
        )
        continue
      }
      throw new DirectOAuthEventError(oauthEventErrorMessage(event))
    }
    if (event.type === 'complete') {
      const status = await getAuthStatus()
      if (!status?.tokens) {
        throw new Error('OAuth completed without an authoritative token set')
      }
      const refreshToken = status.tokens.refreshToken ?? null
      const expiresAt = status.tokens.expiresAt ?? null
      if (
        typeof refreshToken !== 'string' ||
          refreshToken.trim().length === 0 ||
          typeof expiresAt !== 'number' ||
          !Number.isFinite(expiresAt) ||
          expiresAt <= 0
      ) {
        throw new Error(t('native_tui_auth_persistence_failed'))
      }
      const storageResult = await installOAuthTokens({
        accessToken: status.tokens.accessToken,
        refreshToken,
        expiresAt,
        scopes: resolveOAuthCompletionScopes(
          status.tokens.scopes,
          requestedScopes,
        ),
        clientId: status.tokens.clientId,
        serverUrl: status.tokens.serverUrl,
        subscriptionType: null,
        rateLimitTier: null,
        membershipActive: null,
      })
      if (!storageResult.success && !storageResult.committed) {
        throw new Error(t('native_tui_auth_persistence_failed'))
      }
      const orgResult = await validateForceLoginOrg()
      if (!orgResult.valid) throw new Error(orgResult.message)
      // Fixed historical direct-TUI behavior:
      // src/components/ConsoleOAuthFlow.tsx@2358212c:272-280 called the
      // notification service here. The native Rust renderer owns the
      // terminal notification, while the established TypeScript hook side
      // effect remains process-local and does not expand the renderer wire.
      void executeNotificationHooks({
        message: t('oauth_success'),
        notificationType: 'auth_success',
      })
      completed = true
    }
  }
  if (!completed) {
    throw new Error('OAuth stream ended before authentication completed')
  }
  await Promise.all(rendererDeliveries)
}

class DirectOAuthEventError extends Error {
  constructor(message: string) {
    super(message)
    this.name = 'DirectOAuthEventError'
  }
}

function oauthEventErrorMessage(event: {
  err_code?: string
  error?: string
}): string {
  const messages: Record<string, string> = {
    discovery_failed: t('oauth_err_server_unreachable'),
    registration_failed: t('oauth_err_registration'),
    browser_open_failed: t('oauth_err_browser'),
    auth_denied: t('oauth_err_denied'),
    auth_timeout: t('oauth_err_timeout'),
    token_exchange_failed: t('oauth_err_token_exchange'),
    ssl_proxy_detected: t('oauth_err_ssl_proxy'),
  }
  return (
    messages[event.err_code ?? ''] ??
    event.error ??
    t('oauth_err_unknown')
  )
}

async function runProjectMcpApproval(
  requestSetup: DirectTuiSetupRequester,
): Promise<void> {
  if (getSettingsWithAllErrors().errors.length > 0) return
  const pending = Object.keys(getMcpConfigsByScope('project').servers).filter(
    name => getProjectMcpServerStatus(name) === 'pending',
  )
  if (pending.length === 0) return
  const response = await requestSetup(
    {
      ...REQUEST_BASE,
      kind: 'mcp_server_approval',
      title: safeTitle(
        pending.length === 1
          ? t('native_tui_mcp_single_title', {
              name: safeIdentifier(pending[0]!),
            })
          : t('native_tui_mcp_multiple_title', {
              count: pending.length,
            }),
      ),
      body: [safeLine(t('native_tui_mcp_warning'))],
      server_names: pending.map(safeIdentifier),
    },
    CrabCodeTuiMcpServerApprovalResponseSchema,
  )

  if (pending.length === 1) {
    if (response.decision === 'select') {
      throw new Error('MCP select decision is valid only for multiple servers')
    }
    const serverName = pending[0]!
    const historicalChoice =
      response.decision === 'use'
        ? 'yes'
        : response.decision === 'use_all'
          ? 'yes_all'
          : 'no'
    logEvent('tengu_mcp_dialog_choice', {
      choice: historicalChoice as never,
    })
    const current = getInitialSettings()
    if (response.decision === 'use' || response.decision === 'use_all') {
      const enabled = current.enabledMcpjsonServers ?? []
      if (!enabled.includes(serverName)) {
        updateSettingsForSource('localSettings', {
          enabledMcpjsonServers: [...enabled, serverName],
        })
      }
      if (response.decision === 'use_all') {
        updateSettingsForSource('localSettings', {
          enableAllProjectMcpServers: true,
        })
      }
    } else {
      const disabled = current.disabledMcpjsonServers ?? []
      if (!disabled.includes(serverName)) {
        updateSettingsForSource('localSettings', {
          disabledMcpjsonServers: [...disabled, serverName],
        })
      }
    }
    return
  }

  if (response.decision !== 'select' && response.decision !== 'reject') {
    throw new Error(
      'MCP single-server decision is invalid for multiple servers',
    )
  }
  const selected =
    response.decision === 'select'
      ? new Set(response.selected_server_names)
      : new Set<string>()
  if (
    selected.size !==
      (response.decision === 'select'
        ? response.selected_server_names.length
        : 0) ||
    [...selected].some(name => !pending.includes(name))
  ) {
    throw new Error(
      'Native TUI selected an unknown or duplicate project MCP server',
    )
  }
  const approved = pending.filter(name => selected.has(name))
  const rejected = pending.filter(name => !selected.has(name))
  logEvent('tengu_mcp_multidialog_choice', {
    approved: approved.length,
    rejected: rejected.length,
  })
  const current = getInitialSettings()
  if (approved.length > 0) {
    updateSettingsForSource('localSettings', {
      enabledMcpjsonServers: [
        ...new Set([...(current.enabledMcpjsonServers ?? []), ...approved]),
      ],
    })
  }
  if (rejected.length > 0) {
    updateSettingsForSource('localSettings', {
      disabledMcpjsonServers: [
        ...new Set([...(current.disabledMcpjsonServers ?? []), ...rejected]),
      ],
    })
  }
}

async function runExternalCrabcodeMdApproval(
  requestSetup: DirectTuiSetupRequester,
): Promise<void> {
  if (!(await shouldShowCrabcodeMdExternalIncludesWarning())) return
  const includes = getExternalCrabcodeMdIncludes(await getMemoryFiles(true))
  if (includes.length === 0) return
  logEvent('tengu_crabcode_md_includes_dialog_shown', {})
  const response = await requestSetup(
    {
      ...REQUEST_BASE,
      kind: 'external_crabcode_md',
      title: safeTitle(t('native_tui_external_includes_title')),
      body: [safeLine(t('native_tui_external_includes_body'))],
      include_paths: includes.map(include => safePath(include.path)),
      security_url: docsUrl('security'),
    },
    CrabCodeTuiExternalCrabcodeMdResponseSchema,
  )
  logEvent(
    response.decision === 'allow'
      ? 'tengu_crabcode_md_external_includes_dialog_accepted'
      : 'tengu_crabcode_md_external_includes_dialog_declined',
    {},
  )
  saveCurrentProjectConfig(current => ({
    ...current,
    hasCrabcodeMdExternalIncludesApproved: response.decision === 'allow',
    hasCrabcodeMdExternalIncludesWarningShown: true,
  }))
}

async function runStandaloneApiKeyApproval(
  requestSetup: DirectTuiSetupRequester,
): Promise<void> {
  const key = customApiKeyNeedingApproval()
  if (!key) return
  const response = await requestSetup(
    {
      ...REQUEST_BASE,
      kind: 'api_key_approval',
      title: safeTitle(t('native_tui_use_api_key_title')),
      body: [
        safeLine(t('approve_api_key_question')),
        safeLine(`ACOSMI_API_KEY: …${key}`),
      ],
    },
    CrabCodeTuiApiKeyApprovalResponseSchema,
  )
  persistApiKeyDecision(key, response.decision === 'accept')
}

async function runBypassConsent(
  requestSetup: DirectTuiSetupRequester,
  options: DirectTuiSetupLifecycleOptions,
  exit: SetupExit,
): Promise<void> {
  if (
    options.permissionMode !== 'bypassPermissions' &&
    !options.allowDangerouslySkipPermissions
  ) {
    return
  }
  if (hasSkipDangerousModePermissionPrompt()) return
  logEvent('tengu_bypass_permissions_mode_dialog_shown', {})
  const response = await requestSetup(
    {
      ...REQUEST_BASE,
      kind: 'bypass_permissions_consent',
      title: safeTitle(t('native_tui_bypass_title')),
      body: [
        safeLine(t('native_tui_bypass_body')),
        safeLine(t('native_tui_bypass_responsibility')),
      ],
      security_url: docsUrl('security'),
    },
    CrabCodeTuiBypassPermissionsConsentResponseSchema,
  )
  if (response.decision === 'decline') exit(1)
  if (response.decision === 'escape') exit(0)
  logEvent('tengu_bypass_permissions_mode_dialog_accept', {})
  updateSettingsForSource('userSettings', {
    skipDangerousModePermissionPrompt: true,
  })
}

async function runAutoModeConsent(
  requestSetup: DirectTuiSetupRequester,
  options: DirectTuiSetupLifecycleOptions,
  exit: SetupExit,
): Promise<void> {
  if (
    !feature('TRANSCRIPT_CLASSIFIER') ||
    options.permissionMode !== 'auto' ||
    hasAutoModeOptIn()
  ) {
    return
  }
  logEvent('tengu_auto_mode_opt_in_dialog_shown', {})
  const response = await requestSetup(
    {
      ...REQUEST_BASE,
      kind: 'auto_mode_opt_in',
      title: safeTitle(t('native_tui_auto_mode_title')),
      body: [
        safeLine(t('native_tui_auto_mode_body')),
        safeLine(t('native_tui_auto_mode_warning')),
      ],
      security_url: docsUrl('security'),
    },
    CrabCodeTuiAutoModeOptInResponseSchema,
  )
  if (response.decision === 'decline') {
    logEvent('tengu_auto_mode_opt_in_dialog_decline', {})
    exit(1)
  }
  logEvent(
    response.decision === 'accept_default'
      ? 'tengu_auto_mode_opt_in_dialog_accept_default'
      : 'tengu_auto_mode_opt_in_dialog_accept',
    {},
  )
  updateSettingsForSource('userSettings', {
    skipAutoPermissionPrompt: true,
    ...(response.decision === 'accept_default'
      ? { permissions: { defaultMode: 'auto' as const } }
      : {}),
  })
}

async function runDevelopmentChannels(
  requestSetup: DirectTuiSetupRequester,
  options: DirectTuiSetupLifecycleOptions,
  exit: SetupExit,
): Promise<void> {
  if (!(feature('KAIROS') || feature('KAIROS_CHANNELS'))) return
  const devChannels = options.devChannels
  if (getAllowedChannels().length > 0 || devChannels.length > 0) {
    await checkGate_CACHED_OR_BLOCKING('tengu_harbor')
  }
  if (devChannels.length === 0) return
  if (!isChannelsEnabled() || !getAcosmiOAuthTokens()?.accessToken) {
    appendDevelopmentChannels(devChannels)
    return
  }
  const response = await requestSetup(
    {
      ...REQUEST_BASE,
      kind: 'development_channels',
      title: safeTitle(t('native_tui_dev_channels_title')),
      body: [safeLine(t('native_tui_dev_channels_body'))],
      channels: devChannels.map(channel =>
        channel.kind === 'plugin'
          ? {
              kind: channel.kind,
              name: safeIdentifier(channel.name),
              marketplace: safeIdentifier(channel.marketplace),
            }
          : {
              kind: channel.kind,
              name: safeIdentifier(channel.name),
            },
      ),
    },
    CrabCodeTuiDevelopmentChannelsResponseSchema,
  )
  if (response.decision === 'exit') exit(1)
  if (response.decision === 'escape') exit(0)
  appendDevelopmentChannels(devChannels)
}

function createGroveRenderer(
  requestSetup: DirectTuiSetupRequester,
): DirectTuiGroveRenderer {
  return {
    async handleNativeTuiGroveTerms(input) {
      return requestSetup(
        {
          ...REQUEST_BASE,
          kind: 'grove_terms',
          title: safeTitle(input.title),
          body: input.body.map(safeLine),
          links: input.links.map(link => ({
            label: safeLabel(link.label),
            url: link.url,
          })),
          options: input.options.map(option => ({
            decision: option.decision,
            label: safeLabel(option.label),
          })),
        },
        CrabCodeTuiGroveTermsResponseSchema,
      )
    },
  }
}

function customApiKeyNeedingApproval(): string | undefined {
  const apiKey = process.env.ACOSMI_API_KEY
  if (!apiKey || isRunningOnHomespace()) return undefined
  const normalized = normalizeApiKeyForConfig(apiKey)
  return getCustomApiKeyStatus(normalized) === 'new' ? normalized : undefined
}

function persistApiKeyDecision(key: string, approved: boolean): void {
  saveGlobalConfig(current => ({
    ...current,
    customApiKeyResponses: {
      ...current.customApiKeyResponses,
      ...(approved
        ? {
            approved: [...(current.customApiKeyResponses?.approved ?? []), key],
          }
        : {
            rejected: [...(current.customApiKeyResponses?.rejected ?? []), key],
          }),
    },
  }))
}

function appendDevelopmentChannels(channels: ChannelEntry[]): void {
  setAllowedChannels([
    ...getAllowedChannels(),
    ...channels.map(channel => ({ ...channel, dev: true })),
  ])
  setHasDevChannels(true)
}

function oauthOptions(forcedMethod: unknown): Array<{
  value: 'acosmi' | 'console' | 'platform'
  label: string
}> {
  if (forcedMethod === 'acosmi') {
    return [{ value: 'acosmi', label: t('oauth_method_subscription') }]
  }
  if (forcedMethod === 'console') {
    return [{ value: 'console', label: t('oauth_method_console') }]
  }
  return [
    {
      value: 'acosmi',
      label: t('oauth_method_subscription'),
    },
    {
      value: 'console',
      label: t('oauth_method_console'),
    },
    {
      value: 'platform',
      label: t('oauth_method_platform'),
    },
  ]
}

function logOnboardingStep(oauthEnabled: boolean, stepId: string): void {
  logEvent('tengu_onboarding_step', {
    oauthEnabled,
    stepId: stepId as never,
  })
}

function themeLabel(theme: ThemeSetting): string {
  switch (theme) {
    case 'auto':
      return t('theme_auto_match_terminal')
    case 'dark':
      return t('theme_dark_mode')
    case 'light':
      return t('theme_light_mode')
    case 'light-daltonized':
      return t('theme_light_colorblind')
    case 'dark-daltonized':
      return t('theme_dark_colorblind')
    case 'light-ansi':
      return t('theme_light_ansi')
    case 'dark-ansi':
      return t('theme_dark_ansi')
  }
}

function shouldOfferTerminalSetupWithoutUi(): boolean {
  return (
    (process.platform === 'darwin' && env.terminal === 'Apple_Terminal') ||
    env.terminal === 'vscode' ||
    env.terminal === 'cursor' ||
    env.terminal === 'windsurf' ||
    env.terminal === 'alacritty' ||
    env.terminal === 'zed'
  )
}

function safeTitle(value: string): string {
  return safeCopy(value, 160, true)
}

function safeLine(value: string): string {
  return safeCopy(value, 1024, false)
}

function safeLabel(value: string): string {
  return safeCopy(value, 240, true)
}

function safeIdentifier(value: string): string {
  return safeCopy(value, 512, true)
}

function safePath(value: string): string {
  if (
    value.length === 0 ||
    value.length > 4096 ||
    /[\u0000-\u001f\u007f]/.test(value)
  ) {
    throw new Error('Setup authority produced an unsafe external include path')
  }
  return value
}

function safeCopy(
  value: string,
  maxLength: number,
  requireNonempty: boolean,
): string {
  const normalized = value
    .replace(/[\u0000-\u001f\u007f]+/g, ' ')
    .replace(/\s+/g, ' ')
    .trim()
    .slice(0, maxLength)
  if (requireNonempty && normalized.length === 0) {
    throw new Error('Setup authority produced empty renderer copy')
  }
  return normalized
}

function delay(milliseconds: number): Promise<void> {
  return new Promise(resolveDelay => setTimeout(resolveDelay, milliseconds))
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error)
}

// Compile-time guard: every renderer request still belongs to the one closed
// process-private setup union. This function is intentionally never called.
function _assertClosedRequest(request: CrabCodeTuiSetupRequest): void {
  void request
}
