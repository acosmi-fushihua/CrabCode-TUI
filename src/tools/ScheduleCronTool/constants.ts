// Tool-name constants for the cron scheduling tools.
//
// Kept in a dedicated leaf module (zero imports) so that
// `src/constants/tools.ts` can reference the cron tool names without pulling
// in the heavy `prompt.ts` (which imports growthbook / cronTasks). Importing
// these names from `prompt.ts` created a circular import: with
// `LOCAL_AUTOMATIONS` enabled, `IN_PROCESS_TEAMMATE_ALLOWED_TOOLS`
// eagerly evaluates `[CRON_CREATE_TOOL_NAME, ...]` at module init while
// `prompt.ts` is still mid-initialization → TDZ ReferenceError. `prompt.ts`
// re-exports these for back-compat with existing importers.

export const CRON_CREATE_TOOL_NAME = 'CronCreate'
export const CRON_DELETE_TOOL_NAME = 'CronDelete'
export const CRON_LIST_TOOL_NAME = 'CronList'

/**
 * Compile-time release boundary for durable cron consumption.
 *
 * 与 Rust 权威
 * `acosmi-daemon-launcher cron.rs::DURABLE_CRON_LIVE_CONSUMER_RELEASED` 同步。
 * 生命周期清理、Windows spawn 互斥、双锁判活和 PID 身份层均已统一。
 * 投递链 = daemon 单写者 delivery journal（begin/accept）+ 会话键路由 +
 * per-thread 属主锚定，at-least-once。Runtime feature flags 仍是 kill switches, never
 * release authority（`CRABCODE_DISABLE_CRON` / GrowthBook 只能关不能开）。
 */
export const DURABLE_CRON_LIVE_CONSUMER_READY = true
