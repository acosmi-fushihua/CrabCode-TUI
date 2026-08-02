/**
 * Feature flag default configuration table.
 *
 * Each known feature flag is listed here with its default value.
 * In the Bun bundler build, these are compile-time constants injected by the
 * build system. In non-Bun environments, the polyfill reads from:
 *   1. Environment variable CRABCODE_FEATURE_<FLAG_NAME> (1/0/true/false)
 *   2. Config file ~/.crabcode/features.json
 *   3. This default table
 *
 * Flags default to false (disabled) unless explicitly listed otherwise. The
 * exceptions are the GA'd flags — `COORDINATOR_MODE`,
 * `VERIFICATION_AGENT`, `EXTRACT_MEMORIES`, `MEMORY_SEARCH_TOOL`,
 * `LOCAL_AUTOMATIONS`, `DEEP_RESEARCH` (default true; rationale
 * comments inline in the table). This keeps the external (non-Acosmi)
 * build's feature surface minimal, consistent with the Bun bundler's DCE
 * behavior for external builds.
 */

export type FeatureFlagName =
  | 'ABLATION_BASELINE'
  | 'AGENT_MEMORY_SNAPSHOT'
  | 'AGENT_TRIGGERS'
  | 'AGENT_TRIGGERS_REMOTE'
  | 'ALLOW_TEST_VERSIONS'
  | 'ANTI_DISTILLATION_CC'
  | 'AUTO_THEME'
  | 'AWAY_SUMMARY'
  | 'BASH_CLASSIFIER'
  | 'BG_SESSIONS'
  | 'BREAK_CACHE_COMMAND'
  | 'BUDDY'
  | 'BUILDING_CRABCODE_APPS'
  | 'BUILTIN_EXPLORE_PLAN_AGENTS'
  | 'BYOC_ENVIRONMENT_RUNNER'
  | 'CACHED_MICROCOMPACT'
  | 'CHICAGO_MCP'
  | 'COMMIT_ATTRIBUTION'
  | 'COMPACTION_REMINDERS'
  | 'CONNECTOR_TEXT'
  | 'CONTEXT_COLLAPSE'
  | 'COORDINATOR_MODE'
  | 'COWORKER_TYPE_TELEMETRY'
  | 'DAEMON'
  | 'DEEP_RESEARCH'
  | 'DOWNLOAD_USER_SETTINGS'
  | 'DUMP_SYSTEM_PROMPT'
  | 'ENHANCED_TELEMETRY_BETA'
  | 'EXPERIMENTAL_SKILL_SEARCH'
  | 'EXTRACT_MEMORIES'
  | 'FILE_PERSISTENCE'
  | 'FORK_SUBAGENT'
  | 'HARD_FAIL'
  | 'HISTORY_PICKER'
  | 'HISTORY_SNIP'
  | 'HOOK_PROMPTS'
  | 'IS_LIBC_GLIBC'
  | 'IS_LIBC_MUSL'
  | 'KAIROS'
  | 'KAIROS_BRIEF'
  | 'KAIROS_CHANNELS'
  | 'KAIROS_DREAM'
  | 'KAIROS_GITHUB_WEBHOOKS'
  | 'KAIROS_PUSH_NOTIFICATION'
  | 'LOCAL_AUTOMATIONS'
  | 'LODESTONE'
  | 'MCP_RICH_OUTPUT'
  | 'MEMORY_MANAGE_TOOL'
  | 'MEMORY_SEARCH_TOOL'
  | 'MEMORY_SHAPE_TELEMETRY'
  | 'MESSAGE_ACTIONS'
  | 'MONITOR_TOOL'
  | 'NATIVE_CLIENT_ATTESTATION'
  | 'NATIVE_CLIPBOARD_IMAGE'
  | 'NEW_INIT'
  | 'OVERFLOW_TEST_TOOL'
  | 'PERFETTO_TRACING'
  | 'POWERSHELL_AUTO_MODE'
  | 'PROACTIVE'
  | 'PROMPT_CACHE_BREAK_DETECTION'
  | 'QUICK_SEARCH'
  | 'REACTIVE_COMPACT'
  | 'REVIEW_ARTIFACT'
  | 'RUN_SKILL_GENERATOR'
  | 'SELF_HOSTED_RUNNER'
  | 'SHADOW_MODE'
  | 'SHOT_STATS'
  | 'SKILL_IMPROVEMENT'
  | 'SKIP_DETECTION_WHEN_AUTOUPDATES_DISABLED'
  | 'SLOW_OPERATION_LOGGING'
  | 'SSH_REMOTE'
  | 'STREAMLINED_OUTPUT'
  | 'TEAMMEM'
  | 'TEMPLATES'
  | 'TERMINAL_PANEL'
  | 'TOKEN_BUDGET'
  | 'TORCH'
  | 'TRANSCRIPT_CLASSIFIER'
  | 'TREE_SITTER_BASH'
  | 'TREE_SITTER_BASH_SHADOW'
  | 'UDS_INBOX'
  | 'ULTRATHINK'
  | 'UNATTENDED_RETRY'
  | 'UPLOAD_USER_SETTINGS'
  | 'VERIFICATION_AGENT'
  | 'WEB_BROWSER_TOOL'
  | 'WORKFLOW_SCRIPTS'

/**
 * Default values for all feature flags.
 *
 * Every flag defaults to `false` except the GA'd ones — `COORDINATOR_MODE`,
 * `VERIFICATION_AGENT`, `EXTRACT_MEMORIES`,
 * `MEMORY_SEARCH_TOOL` (each has its rationale comment inline
 * in the table below). The Bun bundler's external build already replaces
 * feature() calls with the manifest default for flags not overridden, so
 * this table mirrors that behavior.
 *
 * To enable flags at runtime in non-Bun environments, use either an
 * environment variable or the local features configuration file.
 */
export const FEATURE_FLAG_DEFAULTS: Readonly<Record<FeatureFlagName, boolean>> =
  {
    ABLATION_BASELINE: false,
    AGENT_MEMORY_SNAPSHOT: false,
    AGENT_TRIGGERS: false,
    AGENT_TRIGGERS_REMOTE: false,
    ALLOW_TEST_VERSIONS: false,
    ANTI_DISTILLATION_CC: false,
    AUTO_THEME: false,
    AWAY_SUMMARY: false,
    BASH_CLASSIFIER: false,
    BG_SESSIONS: false,
    BREAK_CACHE_COMMAND: false,
    BUDDY: false,
    BUILDING_CRABCODE_APPS: false,
    BUILTIN_EXPLORE_PLAN_AGENTS: false,
    BYOC_ENVIRONMENT_RUNNER: false,
    CACHED_MICROCOMPACT: false,
    CHICAGO_MCP: false,
    COMMIT_ATTRIBUTION: false,
    COMPACTION_REMINDERS: false,
    CONNECTOR_TEXT: false,
    CONTEXT_COLLAPSE: false,
    // GA 2026-06-10: coordinator mode itself stays opt-in per session via
    // --coordinator / CRABCODE_COORDINATOR_MODE=1; this default only makes the
    // code path reachable. Rollback valve: CRABCODE_FEATURE_COORDINATOR_MODE=0.
    COORDINATOR_MODE: true,
    COWORKER_TYPE_TELEMETRY: false,
    DAEMON: false,
    // GA 2026-07-27 (W-DEEP-RESEARCH-BUILTIN): makes the bundled
    // `deep-research` workflow, its `web-researcher` built-in agent and the
    // /deep-research bundled skill reachable. This is deliberately NOT
    // `WORKFLOW_SCRIPTS`: that flag additionally opens plugin-authored
    // workflow discovery and the /workflows management surface, which stays
    // in private preview. Rollback valve: CRABCODE_FEATURE_DEEP_RESEARCH=0.
    DEEP_RESEARCH: true,
    DOWNLOAD_USER_SETTINGS: false,
    DUMP_SYSTEM_PROMPT: false,
    ENHANCED_TELEMETRY_BETA: false,
    EXPERIMENTAL_SKILL_SEARCH: false,
    // GA 2026-06-11 (W-MEMORY-SELF-EVOLUTION A1): makes the extract-memories
    // code path reachable. Runtime activation is still gated per-user via
    // `extractMemoriesEnabled` in settings.json (default off; enabled by the
    // dream-space first-open consent dialog) with the GrowthBook chain as
    // fallback. Rollback valve: CRABCODE_FEATURE_EXTRACT_MEMORIES=0.
    EXTRACT_MEMORIES: true,
    FILE_PERSISTENCE: false,
    FORK_SUBAGENT: false,
    HARD_FAIL: false,
    HISTORY_PICKER: false,
    HISTORY_SNIP: false,
    HOOK_PROMPTS: false,
    IS_LIBC_GLIBC: false,
    IS_LIBC_MUSL: false,
    KAIROS: false,
    KAIROS_BRIEF: false,
    KAIROS_CHANNELS: false,
    KAIROS_DREAM: false,
    KAIROS_GITHUB_WEBHOOKS: false,
    KAIROS_PUSH_NOTIFICATION: false,
    // Gates the LOCAL cron/automation tool surface — CronCreate/CronList/
    // CronDelete + the /loop skill + the cron tip. Defaults `true` because
    // local cron tooling is GA (it talks only to the bundled crabcode-cron
    // daemon over UDS/Named Pipe, no remote/Acosmi dependency). Was GA'd
    // default-true at f6cc5c84, flipped off by PR-6 (1ef2b80f) while the
    // durable release boundary was closed, and restored by
    // W-CRON-RELEASE-REOPEN (2026-07-16) together with that boundary —
    // see `src/tools/ScheduleCronTool/constants.ts` for the release
    // rationale. Distinct from `AGENT_TRIGGERS_REMOTE`, which gates
    // remote/experimental trigger surface and stays default-false.
    LOCAL_AUTOMATIONS: true,
    LODESTONE: false,
    MCP_RICH_OUTPUT: false,
    // GA 2026-07-16 (W-MEMORY-SYNERGY W7) — the guarded write surface of the
    // memory system (dream_now / knowledge draft / watch list). Every write
    // action goes through the standard tool-permission gate (non-readonly
    // tool) AND its own server-side guardrails (dream gates, drafts land
    // enabled:false pending user review). Rollback valve:
    // CRABCODE_FEATURE_MEMORY_MANAGE_TOOL=0.
    MEMORY_MANAGE_TOOL: true,
    // GA 2026-07-09 (W-MEMORY-LIFECYCLE K7) — the read-only MemorySearchTool
    // ships enabled: saved memory, the user-global memory root, the personal
    // knowledge base, and evolution artifacts are model-searchable by default
    // (previously default-off dark 灰度 from W-MEMORY-DATA-COMPLETION A3).
    // Rollback valve: CRABCODE_FEATURE_MEMORY_SEARCH_TOOL=0.
    MEMORY_SEARCH_TOOL: true,
    MEMORY_SHAPE_TELEMETRY: false,
    MESSAGE_ACTIONS: false,
    MONITOR_TOOL: false,
    NATIVE_CLIENT_ATTESTATION: false,
    NATIVE_CLIPBOARD_IMAGE: false,
    NEW_INIT: false,
    OVERFLOW_TEST_TOOL: false,
    PERFETTO_TRACING: false,
    POWERSHELL_AUTO_MODE: false,
    PROACTIVE: false,
    PROMPT_CACHE_BREAK_DETECTION: false,
    QUICK_SEARCH: false,
    REACTIVE_COMPACT: false,
    REVIEW_ARTIFACT: false,
    RUN_SKILL_GENERATOR: false,
    SELF_HOSTED_RUNNER: false,
    SHADOW_MODE: false,
    SHOT_STATS: false,
    SKILL_IMPROVEMENT: false,
    SKIP_DETECTION_WHEN_AUTOUPDATES_DISABLED: false,
    SLOW_OPERATION_LOGGING: false,
    SSH_REMOTE: false,
    STREAMLINED_OUTPUT: false,
    TEAMMEM: false,
    TEMPLATES: false,
    TERMINAL_PANEL: false,
    TOKEN_BUDGET: false,
    TORCH: false,
    TRANSCRIPT_CLASSIFIER: false,
    TREE_SITTER_BASH: false,
    TREE_SITTER_BASH_SHADOW: false,
    UDS_INBOX: false,
    ULTRATHINK: false,
    UNATTENDED_RETRY: false,
    UPLOAD_USER_SETTINGS: false,
    // GA 2026-06-10 (W-GOAL-SKILL): makes the built-in 'verification' agent
    // and the /goal bundled skill reachable. The always-on verification
    // contract + structural nudges remain additionally gated on GrowthBook
    // tengu_hive_evidence. Rollback valve: CRABCODE_FEATURE_VERIFICATION_AGENT=0.
    VERIFICATION_AGENT: true,
    WEB_BROWSER_TOOL: false,
    // Private preview only. Promotion requires the Workflow isolation,
    // cancellation, permission and end-to-end gates to pass.
    WORKFLOW_SCRIPTS: false,
  } as const
