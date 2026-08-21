import { describe, expect, test } from 'bun:test'
import { readFileSync } from 'node:fs'
import { resolve } from 'node:path'

import {
  CRABCODE_TUI_SETUP_PROTOCOL_VERSION,
  CRABCODE_TUI_SETUP_SUBTYPE,
  CrabCodeTuiMcpServerApprovalResponseSchema,
  CrabCodeTuiOnboardingPreflightRequestSchema,
  CrabCodeTuiOnboardingCustomApiKeyResponseSchema,
  CrabCodeTuiOnboardingCustomProviderResponseSchema,
  CrabCodeTuiOnboardingOAuthBrowserOpenFailedRequestSchema,
  CrabCodeTuiOnboardingOAuthBrowserRequestSchema,
  CrabCodeTuiOnboardingOAuthSelectResponseSchema,
  CrabCodeTuiRendererContextRequestSchema,
  CrabCodeTuiRendererScrollSpeedRequestSchema,
  CrabCodeTuiSetupRequestSchema,
  CrabCodeTuiWorkspaceTrustRequestSchema,
  type CrabCodeTuiSetupRequest,
} from '../../src/cli/crabcodeTuiBridgeProtocol.js'
import {
  runDirectTuiCustomModelSetup,
  usesDirectCustomModelFlow,
  type DirectTuiSetupRequester,
} from '../../src/cli/directTuiSetupLifecycle.js'
import type { ZodType } from 'zod/v4'

const ROOT = resolve(import.meta.dir, '../..')

function source(path: string): string {
  return readFileSync(resolve(ROOT, path), 'utf8')
}

function expectOrdered(haystack: string, markers: string[]): void {
  let cursor = -1
  for (const marker of markers) {
    const next = haystack.indexOf(marker, cursor + 1)
    expect(next, `missing or out-of-order marker: ${marker}`).toBeGreaterThan(
      cursor,
    )
    cursor = next
  }
}

describe('private direct-TUI setup protocol', () => {
  test('is a bounded closed union rather than a generic prompt channel', () => {
    const valid = {
      subtype: CRABCODE_TUI_SETUP_SUBTYPE,
      protocol_version: CRABCODE_TUI_SETUP_PROTOCOL_VERSION,
      kind: 'onboarding',
      stage: 'language',
      title: 'Language',
      body: ['Choose a language'],
      options: [
        { value: 'zh-CN', label: '中文' },
        { value: 'en-US', label: 'English' },
      ],
    } as const

    expect(CrabCodeTuiSetupRequestSchema.safeParse(valid).success).toBe(true)
    expect(
      CrabCodeTuiSetupRequestSchema.safeParse({
        ...valid,
        arbitrary_payload: {},
      }).success,
    ).toBe(false)
    expect(
      CrabCodeTuiSetupRequestSchema.safeParse({
        ...valid,
        kind: 'generic_prompt',
      }).success,
    ).toBe(false)
    expect(
      CrabCodeTuiSetupRequestSchema.safeParse({
        ...valid,
        body: ['unsafe\u0000copy'],
      }).success,
    ).toBe(false)
    expect(
      CrabCodeTuiSetupRequestSchema.safeParse({
        ...valid,
        title: 'x'.repeat(161),
      }).success,
    ).toBe(false)
  })

  test('does not expose account-bridge OAuth and rejects unoffered values', () => {
    const base = {
      protocol_version: CRABCODE_TUI_SETUP_PROTOCOL_VERSION,
      kind: 'onboarding',
      stage: 'oauth',
      phase: 'select_method',
      decision: 'select',
    } as const

    expect(
      CrabCodeTuiOnboardingOAuthSelectResponseSchema.safeParse({
        ...base,
        method: 'acosmi',
      }).success,
    ).toBe(true)
    expect(
      CrabCodeTuiOnboardingOAuthSelectResponseSchema.safeParse({
        ...base,
        method: 'account_bridge',
      }).success,
    ).toBe(false)
  })

  test('omits response and trust facts already fixed by the closed interaction', () => {
    const customProvider = {
      protocol_version: CRABCODE_TUI_SETUP_PROTOCOL_VERSION,
      kind: 'onboarding',
      stage: 'oauth',
      phase: 'custom_provider',
      decision: 'select',
    } as const
    expect(
      CrabCodeTuiOnboardingCustomProviderResponseSchema.safeParse(
        customProvider,
      ).success,
    ).toBe(true)
    expect(
      CrabCodeTuiOnboardingCustomProviderResponseSchema.safeParse({
        ...customProvider,
        provider: 'openai-compatible',
      }).success,
    ).toBe(false)

    const trust = {
      subtype: CRABCODE_TUI_SETUP_SUBTYPE,
      protocol_version: CRABCODE_TUI_SETUP_PROTOCOL_VERSION,
      kind: 'workspace_trust',
    } as const
    expect(
      CrabCodeTuiWorkspaceTrustRequestSchema.safeParse(trust).success,
    ).toBe(true)
    expect(
      CrabCodeTuiWorkspaceTrustRequestSchema.safeParse({
        ...trust,
        cwd: '/already-bound',
      }).success,
    ).toBe(false)
  })

  test('rejects renderer-redundant fixed literals and duplicated display facts', () => {
    const base = {
      subtype: CRABCODE_TUI_SETUP_SUBTYPE,
      protocol_version: CRABCODE_TUI_SETUP_PROTOCOL_VERSION,
    } as const
    const cases = [
      {
        request: {
          ...base,
          kind: 'onboarding',
          stage: 'theme',
          title: 'Theme',
          body: [],
          options: [
            'dark',
            'light',
            'light-daltonized',
            'dark-daltonized',
            'light-ansi',
            'dark-ansi',
          ].map(value => ({ value, label: value })),
          syntax_toggle_enabled: true,
        },
        redundant: { current_theme: 'dark' },
      },
      {
        request: {
          ...base,
          kind: 'onboarding',
          stage: 'theme',
          title: 'Theme',
          body: [],
          options: [
            'dark',
            'light',
            'light-daltonized',
            'dark-daltonized',
            'light-ansi',
            'dark-ansi',
          ].map(value => ({ value, label: value })),
          syntax_toggle_enabled: true,
        },
        redundant: { syntax_highlighting_disabled: false },
      },
      {
        request: {
          ...base,
          kind: 'api_key_approval',
          title: 'Key',
          body: ['ACOSMI_API_KEY: …1234'],
        },
        redundant: { key_suffix: '1234' },
      },
      {
        request: {
          ...base,
          kind: 'onboarding',
          stage: 'oauth',
          phase: 'select_method',
          title: 'Method',
          body: [],
          options: [
            { value: 'acosmi', label: 'Acosmi' },
            { value: 'console', label: 'Console' },
            { value: 'platform', label: 'Platform' },
          ],
        },
        redundant: { forced_method: 'acosmi' },
      },
      {
        request: {
          ...base,
          kind: 'onboarding',
          stage: 'terminal',
          title: 'Terminal setup',
          body: [],
          options: [
            { value: 'install', label: 'Install' },
            { value: 'skip', label: 'Skip' },
          ],
        },
        redundant: { terminal: 'Apple_Terminal' },
      },
      {
        request: {
          ...base,
          kind: 'mcp_server_approval',
          title: 'MCP',
          body: [],
          server_names: ['one'],
        },
        redundant: { mode: 'single' },
      },
      {
        request: {
          ...base,
          kind: 'grove_terms',
          title: 'Terms',
          body: [],
          links: [],
          options: [
            { decision: 'accept_opt_in', label: 'Accept' },
          ],
        },
        redundant: { dismissable: false },
      },
      {
        request: {
          ...base,
          kind: 'onboarding',
          stage: 'oauth',
          phase: 'custom_base_url',
          title: 'URL',
          body: [],
          initial_value: '',
        },
        redundant: { provider: 'openai-compatible' },
      },
      {
        request: {
          ...base,
          kind: 'onboarding',
          stage: 'oauth',
          phase: 'custom_api_key',
          title: 'Key',
          body: [],
        },
        redundant: { masked: true },
      },
      {
        request: {
          ...base,
          kind: 'onboarding',
          stage: 'oauth',
          phase: 'browser_url',
          title: 'Opening',
          body: ['Open manually', '(c to copy)'],
          url: 'https://acosmi.test/login',
        },
        redundant: { copy_shortcut: 'c' },
      },
      {
        request: {
          ...base,
          kind: 'onboarding',
          stage: 'oauth',
          phase: 'error',
          title: 'Failed',
          body: [],
          error: 'denied',
        },
        redundant: { retryable: true },
      },
      {
        request: {
          ...base,
          kind: 'api_key_approval',
          title: 'Key',
          body: ['ACOSMI_API_KEY: …1234'],
        },
        redundant: { key_suffix: '1234' },
      },
    ] as const

    for (const { request, redundant } of cases) {
      expect(CrabCodeTuiSetupRequestSchema.safeParse(request).success).toBe(
        true,
      )
      expect(
        CrabCodeTuiSetupRequestSchema.safeParse({
          ...request,
          ...redundant,
        }).success,
      ).toBe(false)
    }
  })

  test('keeps the historical OAuth browser projection exact and renderer-private', () => {
    const browser = {
      subtype: CRABCODE_TUI_SETUP_SUBTYPE,
      protocol_version: CRABCODE_TUI_SETUP_PROTOCOL_VERSION,
      kind: 'onboarding',
      stage: 'oauth',
      phase: 'browser_url',
      title: 'Opening browser',
      body: ['Open the URL manually', '(c to copy)'],
      url: 'https://acosmi.test/login',
    } as const

    expect(
      CrabCodeTuiOnboardingOAuthBrowserRequestSchema.safeParse(browser).success,
    ).toBe(true)
    for (const body of [
      [],
      ['Open the URL manually'],
      ['Open the URL manually', '(c to copy)', 'invented copy'],
    ]) {
      expect(
        CrabCodeTuiOnboardingOAuthBrowserRequestSchema.safeParse({
          ...browser,
          body,
        }).success,
      ).toBe(false)
    }

    const browserOpenFailed = {
      subtype: CRABCODE_TUI_SETUP_SUBTYPE,
      protocol_version: CRABCODE_TUI_SETUP_PROTOCOL_VERSION,
      kind: 'onboarding',
      stage: 'oauth',
      phase: 'browser_open_failed',
    } as const
    expect(
      CrabCodeTuiOnboardingOAuthBrowserOpenFailedRequestSchema.safeParse(
        browserOpenFailed,
      ).success,
    ).toBe(true)
    expect(
      CrabCodeTuiSetupRequestSchema.safeParse(browserOpenFailed).success,
    ).toBe(true)
    expect(
      CrabCodeTuiOnboardingOAuthBrowserOpenFailedRequestSchema.safeParse({
        ...browserOpenFailed,
        error: 'invented renderer payload',
      }).success,
    ).toBe(false)
  })

  test('has no invented renderer interaction for a successful preflight', () => {
    const failure = {
      subtype: CRABCODE_TUI_SETUP_SUBTYPE,
      protocol_version: CRABCODE_TUI_SETUP_PROTOCOL_VERSION,
      kind: 'onboarding',
      stage: 'preflight',
      title: 'Unable to connect',
      body: ['offline'],
      error: 'offline',
    } as const

    expect(
      CrabCodeTuiOnboardingPreflightRequestSchema.safeParse(failure).success,
    ).toBe(true)
    expect(
      CrabCodeTuiOnboardingPreflightRequestSchema.safeParse({
        ...failure,
        result: 'failure',
      }).success,
    ).toBe(false)
  })

  test('allows an MCP server list only for the select decision', () => {
    const base = {
      protocol_version: CRABCODE_TUI_SETUP_PROTOCOL_VERSION,
      kind: 'mcp_server_approval',
    } as const

    expect(
      CrabCodeTuiMcpServerApprovalResponseSchema.safeParse({
        ...base,
        decision: 'use',
      }).success,
    ).toBe(true)
    expect(
      CrabCodeTuiMcpServerApprovalResponseSchema.safeParse({
        ...base,
        decision: 'use',
        selected_server_names: [],
      }).success,
    ).toBe(false)
    expect(
      CrabCodeTuiMcpServerApprovalResponseSchema.safeParse({
        ...base,
        decision: 'select',
        selected_server_names: ['server-a'],
      }).success,
    ).toBe(true)
    expect(
      CrabCodeTuiMcpServerApprovalResponseSchema.safeParse({
        ...base,
        decision: 'select',
        selected_server_names: [],
      }).success,
    ).toBe(true)
  })

  test('keeps custom API keys masked, control-free and outside arbitrary copy limits', () => {
    const base = {
      protocol_version: CRABCODE_TUI_SETUP_PROTOCOL_VERSION,
      kind: 'onboarding',
      stage: 'oauth',
      phase: 'custom_api_key',
      decision: 'submit',
    } as const

    expect(
      CrabCodeTuiOnboardingCustomApiKeyResponseSchema.parse({
        ...base,
        api_key: `  ${'k'.repeat(12_000)}  `,
      }).api_key,
    ).toHaveLength(12_000)
    expect(
      CrabCodeTuiOnboardingCustomApiKeyResponseSchema.safeParse({
        ...base,
        api_key: 'secret\u0000tail',
      }).success,
    ).toBe(false)
  })

  test('projects only the fixed renderer configuration facts', () => {
    const context = {
      subtype: CRABCODE_TUI_SETUP_SUBTYPE,
      protocol_version: CRABCODE_TUI_SETUP_PROTOCOL_VERSION,
      kind: 'renderer_context',
      cwd: '/workspace',
      config_verbose: false,
      preferred_notification_channel: 'iterm2_with_bell',
      message_idle_notification_threshold_ms: 60_000,
      ui_language: 'zh-CN',
      theme_setting: 'dark',
      syntax_highlighting_disabled: false,
    } as const

    expect(
      CrabCodeTuiRendererContextRequestSchema.safeParse(context).success,
    ).toBe(true)
    for (const preferred_notification_channel of [
      'bell',
      'desktop',
      'unsupported',
    ]) {
      expect(
        CrabCodeTuiRendererContextRequestSchema.safeParse({
          ...context,
          preferred_notification_channel,
        }).success,
      ).toBe(false)
    }
    for (const message_idle_notification_threshold_ms of [-1, 1.5]) {
      expect(
        CrabCodeTuiRendererContextRequestSchema.safeParse({
          ...context,
          message_idle_notification_threshold_ms,
        }).success,
      ).toBe(false)
    }
    for (const ui_language of ['zh', 'en', 'fr-FR']) {
      expect(
        CrabCodeTuiRendererContextRequestSchema.safeParse({
          ...context,
          ui_language,
        }).success,
      ).toBe(false)
    }
    expect(
      CrabCodeTuiRendererContextRequestSchema.safeParse({
        ...context,
        task_complete_notif_enabled: true,
      }).success,
    ).toBe(false)
    for (const theme_setting of ['night', 'unsupported']) {
      expect(
        CrabCodeTuiRendererContextRequestSchema.safeParse({
          ...context,
          theme_setting,
        }).success,
      ).toBe(false)
    }
    for (const field of [
      'theme_setting',
      'syntax_highlighting_disabled',
      'ui_language',
    ] as const) {
      const missing = { ...context } as Record<string, unknown>
      delete missing[field]
      expect(
        CrabCodeTuiRendererContextRequestSchema.safeParse(missing).success,
      ).toBe(false)
    }
  })

  test('keeps the post-trust scroll projection raw, nullable, and closed', () => {
    const request = {
      subtype: CRABCODE_TUI_SETUP_SUBTYPE,
      protocol_version: CRABCODE_TUI_SETUP_PROTOCOL_VERSION,
      kind: 'renderer_scroll_speed',
      raw_value: '\u00a03rows',
    } as const

    expect(
      CrabCodeTuiRendererScrollSpeedRequestSchema.safeParse(request).success,
    ).toBe(true)
    expect(
      CrabCodeTuiRendererScrollSpeedRequestSchema.safeParse({
        ...request,
        raw_value: null,
      }).success,
    ).toBe(true)
    for (const invalid of [
      {
        subtype: CRABCODE_TUI_SETUP_SUBTYPE,
        protocol_version: CRABCODE_TUI_SETUP_PROTOCOL_VERSION,
        kind: 'renderer_scroll_speed',
      },
      { ...request, raw_value: 3 },
      { ...request, settings: { CRABCODE_SCROLL_SPEED: '3' } },
    ]) {
      expect(
        CrabCodeTuiRendererScrollSpeedRequestSchema.safeParse(invalid).success,
      ).toBe(false)
    }
  })
})

type SetupResponseFixture = Record<string, unknown>

function setupRequester(
  responses: SetupResponseFixture[],
  requests: CrabCodeTuiSetupRequest[],
): DirectTuiSetupRequester {
  return async function requestSetup<Response>(
    request: CrabCodeTuiSetupRequest,
    responseSchema: ZodType<Response>,
  ): Promise<Response> {
    requests.push(request)
    const response = responses.shift()
    if (!response) throw new Error('missing setup response fixture')
    return responseSchema.parse(response)
  }
}

function customResponse(
  phase:
    | 'custom_provider'
    | 'custom_base_url'
    | 'custom_model_id'
    | 'custom_api_key',
  body: Record<string, unknown>,
): SetupResponseFixture {
  return {
    protocol_version: CRABCODE_TUI_SETUP_PROTOCOL_VERSION,
    kind: 'onboarding',
    stage: 'oauth',
    phase,
    ...body,
  }
}

describe('fixed historical custom endpoint state machine', () => {
  test('uses the four exact stages and hands one trimmed secret-bearing write to the authority', async () => {
    const requests: CrabCodeTuiSetupRequest[] = []
    const writes: Array<Record<string, string>> = []
    const result = await runDirectTuiCustomModelSetup(
      setupRequester(
        [
          customResponse('custom_provider', {
            decision: 'select',
          }),
          customResponse('custom_base_url', {
            decision: 'submit',
            base_url: '  https://api.example.test/v1  ',
          }),
          customResponse('custom_model_id', {
            decision: 'submit',
            model_id: '  model-a  ',
          }),
          customResponse('custom_api_key', {
            decision: 'submit',
            api_key: '  secret-a  ',
          }),
        ],
        requests,
      ),
      async input => {
        writes.push(input)
      },
    )

    expect(result).toBe(true)
    expect(
      requests.map(request =>
        request.kind === 'onboarding' && request.stage === 'oauth'
          ? request.phase
          : 'unexpected',
      ),
    ).toEqual([
      'custom_provider',
      'custom_base_url',
      'custom_model_id',
      'custom_api_key',
    ])
    expect(writes).toEqual([
      {
        provider: 'openai-compatible',
        baseUrl: 'https://api.example.test/v1',
        modelId: 'model-a',
        apiKey: 'secret-a',
      },
    ])
    const apiKeyRequest = requests.at(-1)
    expect(apiKeyRequest).toMatchObject({
      phase: 'custom_api_key',
    })
    expect(apiKeyRequest).not.toHaveProperty('masked')
    expect(JSON.stringify(apiKeyRequest)).not.toContain('secret-a')
  })

  test('implements the historical Esc back-chain without committing', async () => {
    const requests: CrabCodeTuiSetupRequest[] = []
    let writes = 0
    const result = await runDirectTuiCustomModelSetup(
      setupRequester(
        [
          customResponse('custom_provider', {
            decision: 'select',
          }),
          customResponse('custom_base_url', { decision: 'back' }),
          customResponse('custom_provider', { decision: 'back' }),
        ],
        requests,
      ),
      async () => {
        writes += 1
      },
    )

    expect(result).toBe(false)
    expect(
      requests.map(request =>
        request.kind === 'onboarding' && request.stage === 'oauth'
          ? request.phase
          : 'unexpected',
      ),
    ).toEqual(['custom_provider', 'custom_base_url', 'custom_provider'])
    expect(writes).toBe(0)
  })

  test('does not invent URL policy and keeps write failures on a fresh masked-key phase', async () => {
    const requests: CrabCodeTuiSetupRequest[] = []
    const submittedKeys: string[] = []
    const result = await runDirectTuiCustomModelSetup(
      setupRequester(
        [
          customResponse('custom_provider', {
            decision: 'select',
          }),
          customResponse('custom_base_url', {
            decision: 'submit',
            base_url: 'file:///tmp/model',
          }),
          customResponse('custom_model_id', {
            decision: 'submit',
            model_id: 'model-a',
          }),
          customResponse('custom_api_key', {
            decision: 'submit',
            api_key: 'first-secret',
          }),
          customResponse('custom_api_key', {
            decision: 'submit',
            api_key: 'second-secret',
          }),
        ],
        requests,
      ),
      async input => {
        expect(input.baseUrl).toBe('file:///tmp/model')
        submittedKeys.push(input.apiKey)
        if (submittedKeys.length === 1) {
          throw new Error('secure storage unavailable')
        }
      },
    )

    expect(result).toBe(true)
    expect(
      requests.map(request =>
        request.kind === 'onboarding' && request.stage === 'oauth'
          ? request.phase
          : 'unexpected',
      ),
    ).toEqual([
      'custom_provider',
      'custom_base_url',
      'custom_model_id',
      'custom_api_key',
      'custom_api_key',
    ])
    expect(requests[4]).toMatchObject({
      phase: 'custom_api_key',
      error: 'secure storage unavailable',
    })
    expect(requests[4]).not.toHaveProperty('masked')
    expect(JSON.stringify(requests)).not.toContain('first-secret')
    expect(JSON.stringify(requests)).not.toContain('second-secret')
  })

  test('distinguishes ordinary console configuration from forced console OAuth', () => {
    expect(usesDirectCustomModelFlow('console', undefined)).toBe(true)
    expect(usesDirectCustomModelFlow('console', 'console')).toBe(false)
    expect(usesDirectCustomModelFlow('acosmi', undefined)).toBe(false)
  })
})

describe('historical setup lifecycle placement', () => {
  test('keeps onboarding order and the post-trust authority order explicit', () => {
    const lifecycle = source('src/cli/directTuiSetupLifecycle.ts')
    const preTrust = lifecycle.slice(
      lifecycle.indexOf('export async function runDirectTuiPreTrustOnboarding'),
      lifecycle.indexOf('export async function runDirectTuiPostTrustSetup'),
    )
    expectOrdered(preTrust, [
      "stage: 'language'",
      "stage: 'preflight'",
      "stage: 'theme'",
      "kind: 'api_key_approval'",
      'await runOnboardingOAuth',
      "stage: 'security'",
      "stage: 'terminal'",
    ])
    expectOrdered(lifecycle, [
      "'auto'",
      "'dark'",
      "'light'",
      "'dark-daltonized'",
      "'light-daltonized'",
      "'dark-ansi'",
      "'light-ansi'",
      'syntax_toggle_enabled: syntaxToggleEnabled',
    ])

    const postTrust = lifecycle.slice(
      lifecycle.indexOf('export async function runDirectTuiPostTrustSetup'),
      lifecycle.indexOf('async function runOnboardingOAuth'),
    )
    expectOrdered(postTrust, [
      'await runProjectMcpApproval',
      'await runExternalCrabcodeMdApproval',
      'await afterTrustSideEffects',
      'await authorities.runGrove',
      'await runStandaloneApiKeyApproval',
      'await runBypassConsent',
      'await runAutoModeConsent',
      'await runDevelopmentChannels',
    ])
    expect(postTrust).not.toContain('runChromeOnboarding')
    expect(postTrust).not.toContain("kind: 'setup_complete'")
  })

  test('preserves the platform catalog information step before onboarding advances', () => {
    const lifecycle = source('src/cli/directTuiSetupLifecycle.ts')
    expectOrdered(lifecycle, [
      "if (method === 'platform')",
      "phase: 'platform_setup'",
      "docsUrl('providers-china-region')",
      "docsUrl('providers-global-region')",
      "docsUrl('model-routing')",
    ])
  })

  test('preserves historical setup side effects without upgrading settings errors into startup failures', () => {
    const lifecycle = source('src/cli/directTuiSetupLifecycle.ts')
    expect(lifecycle).not.toContain('if (update.error) throw update.error')
    expectOrdered(lifecycle, [
      "'tengu_oauth_acosmi_forced'",
      "'tengu_oauth_console_forced'",
      "'tengu_oauth_platform_selected'",
      "'tengu_oauth_console_selected'",
      "'tengu_oauth_acosmi_selected'",
      "'tengu_oauth_success'",
    ])
    expect(lifecycle).toContain("'tengu_oauth_flow_start'")
    expect(lifecycle).toContain("'tengu_oauth_error'")
    expectOrdered(lifecycle, [
      'const orgResult = await validateForceLoginOrg()',
      'void executeNotificationHooks({',
      "notificationType: 'auth_success'",
      'completed = true',
    ])
    expect(lifecycle).toContain("'tengu_mcp_dialog_choice'")
    expect(lifecycle).toContain("'tengu_mcp_multidialog_choice'")
    expect(lifecycle).toContain("'tengu_crabcode_md_includes_dialog_shown'")
    expect(lifecycle).toContain(
      "'tengu_crabcode_md_external_includes_dialog_accepted'",
    )
    expect(lifecycle).toContain(
      "'tengu_crabcode_md_external_includes_dialog_declined'",
    )
    expect(lifecycle).toContain("'tengu_auto_mode_opt_in_dialog_decline'")
  })

  test('places final trust between onboarding and all post-trust setup', () => {
    const bootstrap = source('src/cli/tuiRuntimeBootstrap.ts')
    expectOrdered(bootstrap, [
      'await setup(',
      'await rendererSession.bindRendererContext()',
      'await runDirectTuiPreTrustOnboarding',
      'await rendererSession.ensureWorkspaceTrust(getCwd())',
      'await runDirectTuiPostTrustSetup',
      'applyConfigEnvironmentVariables()',
      'await rendererSession.projectRendererScrollSpeed(',
      'await rendererSession.finishSetup()',
      'getDirectTuiCommands(currentCwd)',
      'getCrabCodeMcpConfigs(dynamicMcpPolicyView.allowed)',
      "await connectMcpConfigs(store, regularMcpConfigs, 'regular')",
    ])
  })

  test('keeps the setup bridge private and renderer-only', () => {
    const protocol = source('src/cli/crabcodeTuiBridgeProtocol.ts')
    const lifecycle = source('src/cli/directTuiSetupLifecycle.ts')
    const structuredIo = source('src/cli/structuredIO.ts')
    const queryCore = source('src/cli/print/queryExecutionCore.ts')

    expect(protocol).not.toContain('account_bridge')
    expect(protocol).not.toContain('generic_prompt')
    expect(lifecycle).not.toMatch(/from ['"].*(appServer|AppServer)/)
    expect(lifecycle).not.toMatch(/from ['"].*\.(tsx|jsx)['"]/)
    expect(structuredIo).not.toContain('message as StdoutMessage')
    expect(structuredIo).not.toContain('crabcode_tui_setup')
    expect(structuredIo).not.toContain('handleNativeTuiGroveTerms')
    expect(queryCore).not.toContain('isCrabCodeTuiSetupControlRequest')
    expect(queryCore).not.toContain(
      'runDirectTuiGroveTermsBarrier(structuredIO',
    )
    expect(queryCore).not.toContain('signalNativeTuiStartupReady')
    expect(protocol).not.toContain('setup_complete')
    expect(protocol).not.toContain('startup_ready')
    expect(protocol).toContain("decision: z.literal('rendered')")
  })

})
