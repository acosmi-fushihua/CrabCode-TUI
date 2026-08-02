import { z } from 'zod/v4'

/**
 * Process-private renderer bridge used only between the bundled CrabCode
 * direct runtime child and its Rust TUI parent. It is deliberately outside the
 * public SDK and backend schemas: renderer migration must not add a product or
 * backend protocol capability.
 *
 * The union below is closed. Every interaction has a dedicated `kind` (and,
 * where necessary, a second closed `stage`/`phase` discriminator). Do not add
 * an arbitrary prompt/message DTO here.
 */
export const CRABCODE_TUI_SETUP_SUBTYPE = 'crabcode_tui_setup' as const
export const CRABCODE_TUI_SETUP_PROTOCOL_VERSION = 1 as const

const SafeTitleSchema = z
  .string()
  .min(1)
  .max(160)
  .refine(value => !/[\u0000-\u001f\u007f]/.test(value))
const SafeLineSchema = z
  .string()
  .max(1024)
  .refine(value => !/[\u0000-\u001f\u007f]/.test(value))
const SafeLabelSchema = z
  .string()
  .min(1)
  .max(240)
  .refine(value => !/[\u0000-\u001f\u007f]/.test(value))
const SafeIdentifierSchema = z
  .string()
  .min(1)
  .max(512)
  .refine(value => !/[\u0000-\u001f\u007f]/.test(value))
const SafePathSchema = z
  .string()
  .min(1)
  .max(4096)
  .refine(value => !/[\u0000-\u001f\u007f]/.test(value))
const SafeUrlSchema = z
  .string()
  .max(4096)
  .refine(value => !/[\u0000-\u001f\u007f]/.test(value))
  .pipe(z.url())
const SetupTextInputSchema = z
  .string()
  .refine(value => !/[\u0000-\u001f\u007f]/.test(value))
const SubmittedSetupTextInputSchema = SetupTextInputSchema.transform(value =>
  value.trim(),
).pipe(z.string().min(1))

const SetupRequestBase = {
  subtype: z.literal(CRABCODE_TUI_SETUP_SUBTYPE),
  protocol_version: z.literal(CRABCODE_TUI_SETUP_PROTOCOL_VERSION),
} as const

const SetupResponseBase = {
  protocol_version: z.literal(CRABCODE_TUI_SETUP_PROTOCOL_VERSION),
} as const

const LocalizedCopy = {
  title: SafeTitleSchema,
  body: z.array(SafeLineSchema).max(32),
} as const

const LocalizedOptionSchema = <Values extends [string, ...string[]]>(
  values: Values,
) =>
  z
    .object({
      value: z.enum(values),
      label: SafeLabelSchema,
    })
    .strict()

export const CRABCODE_TUI_NOTIFICATION_CHANNELS = [
  'auto',
  'iterm2',
  'iterm2_with_bell',
  'terminal_bell',
  'kitty',
  'ghostty',
  'notifications_disabled',
] as const

export const CRABCODE_TUI_THEME_SETTINGS = [
  'auto',
  'dark',
  'light',
  'light-daltonized',
  'dark-daltonized',
  'light-ansi',
  'dark-ansi',
] as const

export const CrabCodeTuiRendererContextRequestSchema = z
  .object({
    ...SetupRequestBase,
    kind: z.literal('renderer_context'),
    cwd: SafePathSchema,
    config_verbose: z.boolean(),
    preferred_notification_channel: z.enum(
      CRABCODE_TUI_NOTIFICATION_CHANNELS,
    ),
    message_idle_notification_threshold_ms: z
      .number()
      .int()
      .nonnegative()
      .max(Number.MAX_SAFE_INTEGER),
    ui_language: z.enum(['zh-CN', 'en-US']),
    theme_setting: z.enum(CRABCODE_TUI_THEME_SETTINGS),
    syntax_highlighting_disabled: z.boolean(),
  })
  .strict()
export const CrabCodeTuiRendererContextResponseSchema = z
  .object({
    ...SetupResponseBase,
    kind: z.literal('renderer_context'),
    decision: z.literal('received'),
  })
  .strict()

/**
 * One-time renderer-only projection of the historical direct TUI's
 * CRABCODE_SCROLL_SPEED input. The child emits this only after the managed
 * environment has been applied post-trust. It is intentionally a single
 * nullable raw value rather than a settings/config transport.
 */
export const CrabCodeTuiRendererScrollSpeedRequestSchema = z
  .object({
    ...SetupRequestBase,
    kind: z.literal('renderer_scroll_speed'),
    raw_value: z.string().nullable(),
  })
  .strict()
export const CrabCodeTuiRendererScrollSpeedResponseSchema = z
  .object({
    ...SetupResponseBase,
    kind: z.literal('renderer_scroll_speed'),
    decision: z.literal('received'),
  })
  .strict()

/**
 * Startup session discovery is a process-private value adapter over the
 * existing direct session-storage authority. The Rust side owns only the
 * fixed picker lifecycle; these values never enter the backend/SDK protocol.
 *
 * Catalogs and transcript messages are transferred as bounded base64 chunks
 * so the adapter cannot exceed the Rust transport's exact NDJSON frame limit
 * when a historical title or message is unusually large.
 */
export const CrabCodeTuiSessionPickerEntrySchema = z
  .object({
    id: SafeIdentifierSchema,
    title: SetupTextInputSchema,
    search_text: SetupTextInputSchema,
    metadata: SetupTextInputSchema,
    tag: SetupTextInputSchema.nullable(),
    branch: SetupTextInputSchema.nullable(),
    group_id: SafeIdentifierSchema.nullable(),
    in_current_worktree: z.boolean(),
  })
  .strict()

const SessionPickerTransferChunk = {
  chunk_index: z.number().int().nonnegative().max(Number.MAX_SAFE_INTEGER),
  data_base64: z.string().regex(/^(?:[A-Za-z0-9+/]{4})*(?:[A-Za-z0-9+/]{2}==|[A-Za-z0-9+/]{3}=)?$/),
  final_chunk: z.boolean(),
} as const

export const CrabCodeTuiSessionPickerLoadingRequestSchema = z
  .object({
    ...SetupRequestBase,
    kind: z.literal('session_picker'),
    phase: z.literal('loading'),
    initial_search: SetupTextInputSchema.nullable(),
  })
  .strict()
export const CrabCodeTuiSessionPickerCatalogStartRequestSchema = z
  .object({
    ...SetupRequestBase,
    kind: z.literal('session_picker'),
    phase: z.literal('catalog_start'),
    update: z.enum(['replace', 'append']),
    has_more: z.boolean(),
    all_projects: z.boolean(),
    current_branch: SetupTextInputSchema.nullable(),
    has_multiple_worktrees: z.boolean(),
    rename_enabled: z.boolean(),
  })
  .strict()
export const CrabCodeTuiSessionPickerCatalogChunkRequestSchema = z
  .object({
    ...SetupRequestBase,
    kind: z.literal('session_picker'),
    phase: z.literal('catalog_chunk'),
    ...SessionPickerTransferChunk,
  })
  .strict()
export const CrabCodeTuiSessionPickerCatalogShowRequestSchema = z
  .object({
    ...SetupRequestBase,
    kind: z.literal('session_picker'),
    phase: z.literal('catalog_show'),
  })
  .strict()
export const CrabCodeTuiSessionPickerPreviewStartRequestSchema = z
  .object({
    ...SetupRequestBase,
    kind: z.literal('session_picker'),
    phase: z.literal('preview_start'),
    id: SafeIdentifierSchema,
  })
  .strict()
export const CrabCodeTuiSessionPickerPreviewMessageChunkRequestSchema = z
  .object({
    ...SetupRequestBase,
    kind: z.literal('session_picker'),
    phase: z.literal('preview_message_chunk'),
    id: SafeIdentifierSchema,
    message_index: z.number().int().nonnegative().max(Number.MAX_SAFE_INTEGER),
    ...SessionPickerTransferChunk,
  })
  .strict()
export const CrabCodeTuiSessionPickerPreviewCompleteRequestSchema = z
  .object({
    ...SetupRequestBase,
    kind: z.literal('session_picker'),
    phase: z.literal('preview_complete'),
    id: SafeIdentifierSchema,
    metadata: SetupTextInputSchema,
    message_count: z.number().int().nonnegative().max(Number.MAX_SAFE_INTEGER),
    branch: SetupTextInputSchema.nullable(),
  })
  .strict()
export const CrabCodeTuiSessionPickerPreviewFailedRequestSchema = z
  .object({
    ...SetupRequestBase,
    kind: z.literal('session_picker'),
    phase: z.literal('preview_failed'),
    id: SafeIdentifierSchema,
    error: SafeLineSchema,
  })
  .strict()
export const CrabCodeTuiSessionPickerResolvedRequestSchema = z
  .object({
    ...SetupRequestBase,
    kind: z.literal('session_picker'),
    phase: z.literal('resolved'),
    session_id: SafeIdentifierSchema,
  })
  .strict()
export const CrabCodeTuiSessionPickerCrossProjectRequestSchema = z
  .object({
    ...SetupRequestBase,
    kind: z.literal('session_picker'),
    phase: z.literal('cross_project'),
    // This is copied byte-for-byte from the existing direct authority. Rust
    // sanitizes only the visible rendering and never rewrites clipboard data.
    command: z.string().min(1),
  })
  .strict()

export const CrabCodeTuiSessionPickerAckResponseSchema = z
  .object({
    ...SetupResponseBase,
    kind: z.literal('session_picker'),
    phase: z.enum([
      'loading',
      'catalog_start',
      'catalog_chunk',
      'preview_start',
      'preview_message_chunk',
      'resolved',
    ]),
    decision: z.literal('received'),
  })
  .strict()

export const CrabCodeTuiSessionPickerInteractionResponseSchema = z.union([
  z
    .object({
      ...SetupResponseBase,
      kind: z.literal('session_picker'),
      phase: z.literal('interaction'),
      decision: z.literal('select'),
      id: SafeIdentifierSchema,
    })
    .strict(),
  z
    .object({
      ...SetupResponseBase,
      kind: z.literal('session_picker'),
      phase: z.literal('interaction'),
      decision: z.literal('preview'),
      id: SafeIdentifierSchema,
    })
    .strict(),
  z
    .object({
      ...SetupResponseBase,
      kind: z.literal('session_picker'),
      phase: z.literal('interaction'),
      decision: z.literal('rename'),
      id: SafeIdentifierSchema,
      title: SubmittedSetupTextInputSchema,
    })
    .strict(),
  z
    .object({
      ...SetupResponseBase,
      kind: z.literal('session_picker'),
      phase: z.literal('interaction'),
      decision: z.literal('load_more'),
      count: z.number().int().positive().max(Number.MAX_SAFE_INTEGER),
    })
    .strict(),
  z
    .object({
      ...SetupResponseBase,
      kind: z.literal('session_picker'),
      phase: z.literal('interaction'),
      decision: z.literal('reload'),
      all_projects: z.boolean(),
    })
    .strict(),
  z
    .object({
      ...SetupResponseBase,
      kind: z.literal('session_picker'),
      phase: z.literal('interaction'),
      decision: z.enum(['back', 'cancel']),
    })
    .strict(),
])

export const CrabCodeTuiWorkspaceTrustRequestSchema = z
  .object({
    ...SetupRequestBase,
    kind: z.literal('workspace_trust'),
  })
  .strict()

export const CrabCodeTuiWorkspaceTrustResponseSchema = z
  .object({
    ...SetupResponseBase,
    kind: z.literal('workspace_trust'),
    decision: z.enum(['accept', 'reject']),
  })
  .strict()

const OnboardingLanguageOptionSchema = LocalizedOptionSchema(['zh-CN', 'en-US'])
export const CrabCodeTuiOnboardingLanguageRequestSchema = z
  .object({
    ...SetupRequestBase,
    kind: z.literal('onboarding'),
    stage: z.literal('language'),
    ...LocalizedCopy,
    options: z.array(OnboardingLanguageOptionSchema).length(2),
  })
  .strict()
export const CrabCodeTuiOnboardingLanguageResponseSchema = z.union([
  z
    .object({
      ...SetupResponseBase,
      kind: z.literal('onboarding'),
      stage: z.literal('language'),
      decision: z.literal('select'),
      locale: z.enum(['zh-CN', 'en-US']),
    })
    .strict(),
  z
    .object({
      ...SetupResponseBase,
      kind: z.literal('onboarding'),
      stage: z.literal('language'),
      decision: z.literal('skip'),
    })
    .strict(),
])

export const CrabCodeTuiOnboardingPreflightRequestSchema = z
  .object({
    ...SetupRequestBase,
    kind: z.literal('onboarding'),
    stage: z.literal('preflight'),
    ...LocalizedCopy,
    // The fixed historical PreflightStep advances immediately on success and
    // renders no user interaction. Only the failure notice crosses the
    // process-private renderer boundary.
    error: SafeLineSchema.optional(),
    ssl_hint: SafeLineSchema.optional(),
  })
  .strict()
export const CrabCodeTuiOnboardingPreflightResponseSchema = z
  .object({
    ...SetupResponseBase,
    kind: z.literal('onboarding'),
    stage: z.literal('preflight'),
    // Delivery acknowledgement only. Historical preflight has no user
    // decision and advances automatically when the check settles.
    decision: z.literal('rendered'),
  })
  .strict()

const OnboardingThemeOptionSchema = LocalizedOptionSchema([
  ...CRABCODE_TUI_THEME_SETTINGS,
])
export const CrabCodeTuiOnboardingThemeRequestSchema = z
  .object({
    ...SetupRequestBase,
    kind: z.literal('onboarding'),
    stage: z.literal('theme'),
    ...LocalizedCopy,
    options: z.array(OnboardingThemeOptionSchema).min(6).max(7),
    syntax_toggle_enabled: z.boolean(),
  })
  .strict()
export const CrabCodeTuiOnboardingThemeResponseSchema = z
  .object({
    ...SetupResponseBase,
    kind: z.literal('onboarding'),
    stage: z.literal('theme'),
    decision: z.literal('select'),
    theme: z.enum(CRABCODE_TUI_THEME_SETTINGS),
    syntax_highlighting_disabled: z.boolean(),
  })
  .strict()

export const CRABCODE_TUI_OAUTH_METHODS = [
  'acosmi',
  'console',
  'platform',
] as const
const OnboardingOAuthMethodOptionSchema = LocalizedOptionSchema([
  ...CRABCODE_TUI_OAUTH_METHODS,
])
export const CrabCodeTuiOnboardingOAuthSelectRequestSchema = z
  .object({
    ...SetupRequestBase,
    kind: z.literal('onboarding'),
    stage: z.literal('oauth'),
    phase: z.literal('select_method'),
    ...LocalizedCopy,
    options: z.array(OnboardingOAuthMethodOptionSchema).min(1).max(3),
  })
  .strict()
export const CrabCodeTuiOnboardingOAuthSelectResponseSchema = z
  .object({
    ...SetupResponseBase,
    kind: z.literal('onboarding'),
    stage: z.literal('oauth'),
    phase: z.literal('select_method'),
    decision: z.literal('select'),
    method: z.enum(CRABCODE_TUI_OAUTH_METHODS),
  })
  .strict()

const OnboardingCustomProviderOptionSchema = LocalizedOptionSchema([
  'openai-compatible',
])
export const CrabCodeTuiOnboardingCustomProviderRequestSchema = z
  .object({
    ...SetupRequestBase,
    kind: z.literal('onboarding'),
    stage: z.literal('oauth'),
    phase: z.literal('custom_provider'),
    ...LocalizedCopy,
    options: z.tuple([OnboardingCustomProviderOptionSchema]),
  })
  .strict()
export const CrabCodeTuiOnboardingCustomProviderResponseSchema = z.union([
  z
    .object({
      ...SetupResponseBase,
      kind: z.literal('onboarding'),
      stage: z.literal('oauth'),
      phase: z.literal('custom_provider'),
      decision: z.literal('select'),
    })
    .strict(),
  z
    .object({
      ...SetupResponseBase,
      kind: z.literal('onboarding'),
      stage: z.literal('oauth'),
      phase: z.literal('custom_provider'),
      decision: z.literal('back'),
    })
    .strict(),
])

export const CrabCodeTuiOnboardingCustomBaseUrlRequestSchema = z
  .object({
    ...SetupRequestBase,
    kind: z.literal('onboarding'),
    stage: z.literal('oauth'),
    phase: z.literal('custom_base_url'),
    ...LocalizedCopy,
    initial_value: SetupTextInputSchema,
  })
  .strict()
export const CrabCodeTuiOnboardingCustomBaseUrlResponseSchema = z.union([
  z
    .object({
      ...SetupResponseBase,
      kind: z.literal('onboarding'),
      stage: z.literal('oauth'),
      phase: z.literal('custom_base_url'),
      decision: z.literal('submit'),
      base_url: SubmittedSetupTextInputSchema,
    })
    .strict(),
  z
    .object({
      ...SetupResponseBase,
      kind: z.literal('onboarding'),
      stage: z.literal('oauth'),
      phase: z.literal('custom_base_url'),
      decision: z.literal('back'),
    })
    .strict(),
])

export const CrabCodeTuiOnboardingCustomModelIdRequestSchema = z
  .object({
    ...SetupRequestBase,
    kind: z.literal('onboarding'),
    stage: z.literal('oauth'),
    phase: z.literal('custom_model_id'),
    ...LocalizedCopy,
    initial_value: SetupTextInputSchema,
  })
  .strict()
export const CrabCodeTuiOnboardingCustomModelIdResponseSchema = z.union([
  z
    .object({
      ...SetupResponseBase,
      kind: z.literal('onboarding'),
      stage: z.literal('oauth'),
      phase: z.literal('custom_model_id'),
      decision: z.literal('submit'),
      model_id: SubmittedSetupTextInputSchema,
    })
    .strict(),
  z
    .object({
      ...SetupResponseBase,
      kind: z.literal('onboarding'),
      stage: z.literal('oauth'),
      phase: z.literal('custom_model_id'),
      decision: z.literal('back'),
    })
    .strict(),
])

export const CrabCodeTuiOnboardingCustomApiKeyRequestSchema = z
  .object({
    ...SetupRequestBase,
    kind: z.literal('onboarding'),
    stage: z.literal('oauth'),
    phase: z.literal('custom_api_key'),
    ...LocalizedCopy,
    error: SafeLineSchema.optional(),
  })
  .strict()
export const CrabCodeTuiOnboardingCustomApiKeyResponseSchema = z.union([
  z
    .object({
      ...SetupResponseBase,
      kind: z.literal('onboarding'),
      stage: z.literal('oauth'),
      phase: z.literal('custom_api_key'),
      decision: z.literal('submit'),
      api_key: SubmittedSetupTextInputSchema,
    })
    .strict(),
  z
    .object({
      ...SetupResponseBase,
      kind: z.literal('onboarding'),
      stage: z.literal('oauth'),
      phase: z.literal('custom_api_key'),
      decision: z.literal('back'),
    })
    .strict(),
])

export const CrabCodeTuiOnboardingOAuthBrowserRequestSchema = z
  .object({
    ...SetupRequestBase,
    kind: z.literal('onboarding'),
    stage: z.literal('oauth'),
    phase: z.literal('browser_url'),
    title: SafeTitleSchema,
    body: z.tuple([SafeLineSchema, SafeLineSchema]),
    url: SafeUrlSchema,
  })
  .strict()
export const CrabCodeTuiOnboardingOAuthBrowserResponseSchema = z
  .object({
    ...SetupResponseBase,
    kind: z.literal('onboarding'),
    stage: z.literal('oauth'),
    phase: z.literal('browser_url'),
    // Delivery acknowledgement only; OAuth streaming must continue without
    // waiting for a user action on this frame.
    decision: z.literal('rendered'),
  })
  .strict()

/**
 * Renderer-only projection of the historical non-fatal
 * `browser_open_failed` event. The backend OAuth event remains unchanged;
 * this closed status merely advances the already-delivered browser prompt
 * from its three-second spinner state to the manual URL state.
 */
export const CrabCodeTuiOnboardingOAuthBrowserOpenFailedRequestSchema = z
  .object({
    ...SetupRequestBase,
    kind: z.literal('onboarding'),
    stage: z.literal('oauth'),
    phase: z.literal('browser_open_failed'),
  })
  .strict()
export const CrabCodeTuiOnboardingOAuthBrowserOpenFailedResponseSchema = z
  .object({
    ...SetupResponseBase,
    kind: z.literal('onboarding'),
    stage: z.literal('oauth'),
    phase: z.literal('browser_open_failed'),
    decision: z.literal('rendered'),
  })
  .strict()

export const CrabCodeTuiOnboardingOAuthPlatformRequestSchema = z
  .object({
    ...SetupRequestBase,
    kind: z.literal('onboarding'),
    stage: z.literal('oauth'),
    phase: z.literal('platform_setup'),
    ...LocalizedCopy,
    links: z
      .array(
        z
          .object({
            label: SafeLabelSchema,
            url: SafeUrlSchema,
          })
          .strict(),
      )
      .length(3),
  })
  .strict()
export const CrabCodeTuiOnboardingOAuthPlatformResponseSchema = z
  .object({
    ...SetupResponseBase,
    kind: z.literal('onboarding'),
    stage: z.literal('oauth'),
    phase: z.literal('platform_setup'),
    decision: z.literal('continue'),
  })
  .strict()

export const CrabCodeTuiOnboardingOAuthSuccessRequestSchema = z
  .object({
    ...SetupRequestBase,
    kind: z.literal('onboarding'),
    stage: z.literal('oauth'),
    phase: z.literal('success'),
    ...LocalizedCopy,
  })
  .strict()
export const CrabCodeTuiOnboardingOAuthSuccessResponseSchema = z
  .object({
    ...SetupResponseBase,
    kind: z.literal('onboarding'),
    stage: z.literal('oauth'),
    phase: z.literal('success'),
    decision: z.literal('continue'),
  })
  .strict()

export const CrabCodeTuiOnboardingOAuthErrorRequestSchema = z
  .object({
    ...SetupRequestBase,
    kind: z.literal('onboarding'),
    stage: z.literal('oauth'),
    phase: z.literal('error'),
    ...LocalizedCopy,
    error: SafeLineSchema,
  })
  .strict()
export const CrabCodeTuiOnboardingOAuthErrorResponseSchema = z
  .object({
    ...SetupResponseBase,
    kind: z.literal('onboarding'),
    stage: z.literal('oauth'),
    phase: z.literal('error'),
    decision: z.literal('retry'),
  })
  .strict()

export const CrabCodeTuiOnboardingSecurityRequestSchema = z
  .object({
    ...SetupRequestBase,
    kind: z.literal('onboarding'),
    stage: z.literal('security'),
    ...LocalizedCopy,
    security_url: SafeUrlSchema,
  })
  .strict()
export const CrabCodeTuiOnboardingSecurityResponseSchema = z
  .object({
    ...SetupResponseBase,
    kind: z.literal('onboarding'),
    stage: z.literal('security'),
    decision: z.literal('continue'),
  })
  .strict()

const OnboardingTerminalOptionSchema = LocalizedOptionSchema([
  'install',
  'skip',
])
export const CrabCodeTuiOnboardingTerminalRequestSchema = z
  .object({
    ...SetupRequestBase,
    kind: z.literal('onboarding'),
    stage: z.literal('terminal'),
    ...LocalizedCopy,
    options: z.array(OnboardingTerminalOptionSchema).length(2),
  })
  .strict()
export const CrabCodeTuiOnboardingTerminalResponseSchema = z
  .object({
    ...SetupResponseBase,
    kind: z.literal('onboarding'),
    stage: z.literal('terminal'),
    decision: z.enum(['install', 'skip']),
  })
  .strict()

export const CrabCodeTuiOnboardingRequestSchema = z.union([
  CrabCodeTuiOnboardingLanguageRequestSchema,
  CrabCodeTuiOnboardingPreflightRequestSchema,
  CrabCodeTuiOnboardingThemeRequestSchema,
  CrabCodeTuiOnboardingOAuthSelectRequestSchema,
  CrabCodeTuiOnboardingCustomProviderRequestSchema,
  CrabCodeTuiOnboardingCustomBaseUrlRequestSchema,
  CrabCodeTuiOnboardingCustomModelIdRequestSchema,
  CrabCodeTuiOnboardingCustomApiKeyRequestSchema,
  CrabCodeTuiOnboardingOAuthBrowserRequestSchema,
  CrabCodeTuiOnboardingOAuthBrowserOpenFailedRequestSchema,
  CrabCodeTuiOnboardingOAuthPlatformRequestSchema,
  CrabCodeTuiOnboardingOAuthSuccessRequestSchema,
  CrabCodeTuiOnboardingOAuthErrorRequestSchema,
  CrabCodeTuiOnboardingSecurityRequestSchema,
  CrabCodeTuiOnboardingTerminalRequestSchema,
])
export const CrabCodeTuiOnboardingResponseSchema = z.union([
  CrabCodeTuiOnboardingLanguageResponseSchema,
  CrabCodeTuiOnboardingPreflightResponseSchema,
  CrabCodeTuiOnboardingThemeResponseSchema,
  CrabCodeTuiOnboardingOAuthSelectResponseSchema,
  CrabCodeTuiOnboardingCustomProviderResponseSchema,
  CrabCodeTuiOnboardingCustomBaseUrlResponseSchema,
  CrabCodeTuiOnboardingCustomModelIdResponseSchema,
  CrabCodeTuiOnboardingCustomApiKeyResponseSchema,
  CrabCodeTuiOnboardingOAuthBrowserResponseSchema,
  CrabCodeTuiOnboardingOAuthBrowserOpenFailedResponseSchema,
  CrabCodeTuiOnboardingOAuthPlatformResponseSchema,
  CrabCodeTuiOnboardingOAuthSuccessResponseSchema,
  CrabCodeTuiOnboardingOAuthErrorResponseSchema,
  CrabCodeTuiOnboardingSecurityResponseSchema,
  CrabCodeTuiOnboardingTerminalResponseSchema,
])

export const CrabCodeTuiMcpServerApprovalRequestSchema = z
  .object({
    ...SetupRequestBase,
    kind: z.literal('mcp_server_approval'),
    ...LocalizedCopy,
    server_names: z.array(SafeIdentifierSchema).min(1).max(256),
  })
  .strict()
export const CrabCodeTuiMcpServerApprovalResponseSchema = z.union([
  z
    .object({
      ...SetupResponseBase,
      kind: z.literal('mcp_server_approval'),
      decision: z.enum(['use', 'use_all', 'reject']),
    })
    .strict(),
  z
    .object({
      ...SetupResponseBase,
      kind: z.literal('mcp_server_approval'),
      decision: z.literal('select'),
      selected_server_names: z.array(SafeIdentifierSchema).max(256),
    })
    .strict(),
])

export const CrabCodeTuiExternalCrabcodeMdRequestSchema = z
  .object({
    ...SetupRequestBase,
    kind: z.literal('external_crabcode_md'),
    ...LocalizedCopy,
    include_paths: z.array(SafePathSchema).min(1).max(256),
    security_url: SafeUrlSchema,
  })
  .strict()
export const CrabCodeTuiExternalCrabcodeMdResponseSchema = z
  .object({
    ...SetupResponseBase,
    kind: z.literal('external_crabcode_md'),
    decision: z.enum(['allow', 'deny']),
  })
  .strict()

export const CrabCodeTuiGroveTermsRequestSchema = z
  .object({
    ...SetupRequestBase,
    kind: z.literal('grove_terms'),
    ...LocalizedCopy,
    links: z
      .array(
        z
          .object({
            label: SafeLabelSchema,
            url: SafeUrlSchema,
          })
          .strict(),
      )
      .max(8),
    options: z
      .array(
        z
          .object({
            decision: z.enum(['accept_opt_in', 'accept_opt_out', 'defer']),
            label: SafeLabelSchema,
          })
          .strict(),
      )
      .min(1)
      .max(3),
  })
  .strict()
export const CrabCodeTuiGroveTermsResponseSchema = z
  .object({
    ...SetupResponseBase,
    kind: z.literal('grove_terms'),
    decision: z.enum(['accept_opt_in', 'accept_opt_out', 'defer', 'escape']),
  })
  .strict()

export const CrabCodeTuiApiKeyApprovalRequestSchema = z
  .object({
    ...SetupRequestBase,
    kind: z.literal('api_key_approval'),
    ...LocalizedCopy,
  })
  .strict()
export const CrabCodeTuiApiKeyApprovalResponseSchema = z
  .object({
    ...SetupResponseBase,
    kind: z.literal('api_key_approval'),
    decision: z.enum(['accept', 'reject']),
  })
  .strict()

export const CrabCodeTuiBypassPermissionsConsentRequestSchema = z
  .object({
    ...SetupRequestBase,
    kind: z.literal('bypass_permissions_consent'),
    ...LocalizedCopy,
    security_url: SafeUrlSchema,
  })
  .strict()
export const CrabCodeTuiBypassPermissionsConsentResponseSchema = z
  .object({
    ...SetupResponseBase,
    kind: z.literal('bypass_permissions_consent'),
    decision: z.enum(['accept', 'decline', 'escape']),
  })
  .strict()

export const CrabCodeTuiAutoModeOptInRequestSchema = z
  .object({
    ...SetupRequestBase,
    kind: z.literal('auto_mode_opt_in'),
    ...LocalizedCopy,
    security_url: SafeUrlSchema,
  })
  .strict()
export const CrabCodeTuiAutoModeOptInResponseSchema = z
  .object({
    ...SetupResponseBase,
    kind: z.literal('auto_mode_opt_in'),
    decision: z.enum(['accept', 'accept_default', 'decline']),
  })
  .strict()

const DevelopmentChannelSchema = z
  .object({
    kind: z.enum(['plugin', 'server']),
    name: SafeIdentifierSchema,
    marketplace: SafeIdentifierSchema.optional(),
  })
  .strict()
export const CrabCodeTuiDevelopmentChannelsRequestSchema = z
  .object({
    ...SetupRequestBase,
    kind: z.literal('development_channels'),
    ...LocalizedCopy,
    channels: z.array(DevelopmentChannelSchema).min(1).max(256),
  })
  .strict()
export const CrabCodeTuiDevelopmentChannelsResponseSchema = z
  .object({
    ...SetupResponseBase,
    kind: z.literal('development_channels'),
    decision: z.enum(['accept', 'exit', 'escape']),
  })
  .strict()

export const CrabCodeTuiSetupRequestSchema = z.union([
  CrabCodeTuiRendererContextRequestSchema,
  CrabCodeTuiRendererScrollSpeedRequestSchema,
  CrabCodeTuiSessionPickerLoadingRequestSchema,
  CrabCodeTuiSessionPickerCatalogStartRequestSchema,
  CrabCodeTuiSessionPickerCatalogChunkRequestSchema,
  CrabCodeTuiSessionPickerCatalogShowRequestSchema,
  CrabCodeTuiSessionPickerPreviewStartRequestSchema,
  CrabCodeTuiSessionPickerPreviewMessageChunkRequestSchema,
  CrabCodeTuiSessionPickerPreviewCompleteRequestSchema,
  CrabCodeTuiSessionPickerPreviewFailedRequestSchema,
  CrabCodeTuiSessionPickerResolvedRequestSchema,
  CrabCodeTuiSessionPickerCrossProjectRequestSchema,
  CrabCodeTuiWorkspaceTrustRequestSchema,
  CrabCodeTuiOnboardingLanguageRequestSchema,
  CrabCodeTuiOnboardingPreflightRequestSchema,
  CrabCodeTuiOnboardingThemeRequestSchema,
  CrabCodeTuiOnboardingOAuthSelectRequestSchema,
  CrabCodeTuiOnboardingCustomProviderRequestSchema,
  CrabCodeTuiOnboardingCustomBaseUrlRequestSchema,
  CrabCodeTuiOnboardingCustomModelIdRequestSchema,
  CrabCodeTuiOnboardingCustomApiKeyRequestSchema,
  CrabCodeTuiOnboardingOAuthBrowserRequestSchema,
  CrabCodeTuiOnboardingOAuthBrowserOpenFailedRequestSchema,
  CrabCodeTuiOnboardingOAuthPlatformRequestSchema,
  CrabCodeTuiOnboardingOAuthSuccessRequestSchema,
  CrabCodeTuiOnboardingOAuthErrorRequestSchema,
  CrabCodeTuiOnboardingSecurityRequestSchema,
  CrabCodeTuiOnboardingTerminalRequestSchema,
  CrabCodeTuiMcpServerApprovalRequestSchema,
  CrabCodeTuiExternalCrabcodeMdRequestSchema,
  CrabCodeTuiGroveTermsRequestSchema,
  CrabCodeTuiApiKeyApprovalRequestSchema,
  CrabCodeTuiBypassPermissionsConsentRequestSchema,
  CrabCodeTuiAutoModeOptInRequestSchema,
  CrabCodeTuiDevelopmentChannelsRequestSchema,
])

export const CrabCodeTuiSetupResponseSchema = z.union([
  CrabCodeTuiRendererContextResponseSchema,
  CrabCodeTuiRendererScrollSpeedResponseSchema,
  CrabCodeTuiSessionPickerAckResponseSchema,
  CrabCodeTuiSessionPickerInteractionResponseSchema,
  CrabCodeTuiWorkspaceTrustResponseSchema,
  CrabCodeTuiOnboardingLanguageResponseSchema,
  CrabCodeTuiOnboardingPreflightResponseSchema,
  CrabCodeTuiOnboardingThemeResponseSchema,
  CrabCodeTuiOnboardingOAuthSelectResponseSchema,
  CrabCodeTuiOnboardingCustomProviderResponseSchema,
  CrabCodeTuiOnboardingCustomBaseUrlResponseSchema,
  CrabCodeTuiOnboardingCustomModelIdResponseSchema,
  CrabCodeTuiOnboardingCustomApiKeyResponseSchema,
  CrabCodeTuiOnboardingOAuthBrowserResponseSchema,
  CrabCodeTuiOnboardingOAuthBrowserOpenFailedResponseSchema,
  CrabCodeTuiOnboardingOAuthPlatformResponseSchema,
  CrabCodeTuiOnboardingOAuthSuccessResponseSchema,
  CrabCodeTuiOnboardingOAuthErrorResponseSchema,
  CrabCodeTuiOnboardingSecurityResponseSchema,
  CrabCodeTuiOnboardingTerminalResponseSchema,
  CrabCodeTuiMcpServerApprovalResponseSchema,
  CrabCodeTuiExternalCrabcodeMdResponseSchema,
  CrabCodeTuiGroveTermsResponseSchema,
  CrabCodeTuiApiKeyApprovalResponseSchema,
  CrabCodeTuiBypassPermissionsConsentResponseSchema,
  CrabCodeTuiAutoModeOptInResponseSchema,
  CrabCodeTuiDevelopmentChannelsResponseSchema,
])

export type CrabCodeTuiRendererContextRequest = z.infer<
  typeof CrabCodeTuiRendererContextRequestSchema
>
export type CrabCodeTuiRendererScrollSpeedRequest = z.infer<
  typeof CrabCodeTuiRendererScrollSpeedRequestSchema
>
export type CrabCodeTuiSessionPickerEntry = z.infer<
  typeof CrabCodeTuiSessionPickerEntrySchema
>
export type CrabCodeTuiSessionPickerInteractionResponse = z.infer<
  typeof CrabCodeTuiSessionPickerInteractionResponseSchema
>
export type CrabCodeTuiWorkspaceTrustRequest = z.infer<
  typeof CrabCodeTuiWorkspaceTrustRequestSchema
>
export type CrabCodeTuiWorkspaceTrustResponse = z.infer<
  typeof CrabCodeTuiWorkspaceTrustResponseSchema
>
export type CrabCodeTuiOnboardingRequest = z.infer<
  typeof CrabCodeTuiOnboardingRequestSchema
>
export type CrabCodeTuiOnboardingResponse = z.infer<
  typeof CrabCodeTuiOnboardingResponseSchema
>
export type CrabCodeTuiMcpServerApprovalRequest = z.infer<
  typeof CrabCodeTuiMcpServerApprovalRequestSchema
>
export type CrabCodeTuiExternalCrabcodeMdRequest = z.infer<
  typeof CrabCodeTuiExternalCrabcodeMdRequestSchema
>
export type CrabCodeTuiGroveTermsRequest = z.infer<
  typeof CrabCodeTuiGroveTermsRequestSchema
>
export type CrabCodeTuiGroveTermsResponse = z.infer<
  typeof CrabCodeTuiGroveTermsResponseSchema
>
export type CrabCodeTuiApiKeyApprovalRequest = z.infer<
  typeof CrabCodeTuiApiKeyApprovalRequestSchema
>
export type CrabCodeTuiBypassPermissionsConsentRequest = z.infer<
  typeof CrabCodeTuiBypassPermissionsConsentRequestSchema
>
export type CrabCodeTuiAutoModeOptInRequest = z.infer<
  typeof CrabCodeTuiAutoModeOptInRequestSchema
>
export type CrabCodeTuiDevelopmentChannelsRequest = z.infer<
  typeof CrabCodeTuiDevelopmentChannelsRequestSchema
>
export type CrabCodeTuiSetupRequest = z.infer<
  typeof CrabCodeTuiSetupRequestSchema
>
export type CrabCodeTuiSetupResponse = z.infer<
  typeof CrabCodeTuiSetupResponseSchema
>

export type CrabCodeTuiSetupControlRequest = {
  type: 'control_request'
  request_id: string
  request: CrabCodeTuiSetupRequest
}
