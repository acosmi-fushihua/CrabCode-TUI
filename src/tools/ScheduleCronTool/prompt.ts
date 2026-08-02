import { feature } from '../../utils/featurePolyfill.js'
import { getFeatureValue_CACHED_WITH_REFRESH } from '../../services/analytics/growthbook.js'
import { DEFAULT_CRON_JITTER_CONFIG } from '../../utils/cronTasks.js'
import { isEnvTruthy } from '../../utils/envUtils.js'
import {
  CRON_CREATE_TOOL_NAME,
  CRON_DELETE_TOOL_NAME,
  CRON_LIST_TOOL_NAME,
  DURABLE_CRON_LIVE_CONSUMER_READY,
} from './constants.js'

const KAIROS_CRON_REFRESH_MS = 5 * 60 * 1000

export const DEFAULT_MAX_AGE_DAYS =
  DEFAULT_CRON_JITTER_CONFIG.recurringMaxAgeMs / (24 * 60 * 60 * 1000)

/**
 * Unified gate for the cron scheduling system. Combines the build-time
 * `feature('LOCAL_AUTOMATIONS')` flag (dead code elimination) with the
 * runtime `tengu_kairos_cron` GrowthBook gate on a 5-minute refresh window.
 *
 * LOCAL_AUTOMATIONS gates the LOCAL cron/automation surface only — distinct
 * from `AGENT_TRIGGERS_REMOTE` (remote triggers). It is independently
 * shippable from KAIROS: the cron module graph (cronTasks/cronTasksLock/
 * cron.ts + the three tools + /loop skill) has zero imports into
 * src/assistant/ and no feature('KAIROS') calls. 调度核心已迁移到 Rust
 * SchedulerDaemon (crabcode-cron daemon over UDS).
 *
 * Called from Tool.isEnabled() (lazy, post-init) and inside useEffect /
 * imperative setup, never at module scope — so the disk cache has had a
 * chance to populate.
 *
 * `LOCAL_AUTOMATIONS` defaults true（GA；W-CRON-RELEASE-REOPEN 2026-07-16
 * 随发布门重开恢复，f6cc5c84 原始 GA 裁决）。The GrowthBook gate remains a
 * fleet kill switch; `CRABCODE_DISABLE_CRON` is the local kill switch and
 * wins over everything.
 * GrowthBook is disabled for Bedrock/Vertex/Foundry and when
 * DISABLE_TELEMETRY / CRABCODE_DISABLE_NONESSENTIAL_TRAFFIC are set.
 * GB serves as a fleet-wide kill switch — flipping it to
 * `false` stops already-running schedulers on their next isKilled poll
 * tick, not just new ones.
 *
 * `CRABCODE_DISABLE_CRON` is a local override that wins over GB.
 */
export function isKairosCronEnabled(): boolean {
  return feature('LOCAL_AUTOMATIONS')
    ? !isEnvTruthy(process.env.CRABCODE_DISABLE_CRON) &&
        getFeatureValue_CACHED_WITH_REFRESH(
          'tengu_kairos_cron',
          true,
          KAIROS_CRON_REFRESH_MS,
        )
    : false
}

/**
 * Kill switch for disk-persistent (durable) cron tasks. Narrower than
 * {@link isKairosCronEnabled}. When this gate is off, durable requests are
 * rejected as unsupported; they are never weakened to session-only work.
 *
 * W-CRON-RELEASE-REOPEN（2026-07-16）：随发布门翻开，GB fallback 恢复
 * pre-1ef2b80f 的 `true`（GB 不可达/关闭时 durable 默认可用；GB 仍是舰队级
 * kill switch，可远程关停）。编译期 READY 守卫保留——未来再关门时这里自动
 * 跟随失效，无需二次改动。仍要求 {@link isKairosCronEnabled}（工具族总闸），
 * 工具族关闭时任何直呼方都拿不到 durable 调度。
 */
export function isDurableCronEnabled(): boolean {
  // Release authority is deliberately compile-time. This early return also
  // prevents truthy env/config/GB overrides from even being consulted while
  // the release boundary is closed.
  if (!DURABLE_CRON_LIVE_CONSUMER_READY) return false
  return (
    isKairosCronEnabled() &&
    getFeatureValue_CACHED_WITH_REFRESH(
      'tengu_kairos_cron_durable',
      true,
      KAIROS_CRON_REFRESH_MS,
    )
  )
}

// Re-exported (imported at the top) from the leaf `constants.ts` module — see
// that file for why the tool-name constants must not live in this (heavy)
// module. Existing importers of `./prompt.js` keep working unchanged.
export {
  CRON_CREATE_TOOL_NAME,
  CRON_DELETE_TOOL_NAME,
  CRON_LIST_TOOL_NAME,
  DURABLE_CRON_LIVE_CONSUMER_READY,
}

export function buildCronCreateDescription(durableEnabled: boolean): string {
  return durableEnabled
    ? 'Schedule a prompt to run at a future time — either recurring on a cron schedule, or once at a specific time. Pass durable: true to persist the job via the cron daemon so it survives restarts; otherwise session-only.'
    : 'Experimental: schedule a prompt within this CrabCode session only. Durable and absolute-time (`at`) schedules are unavailable until the durable live-consumer cutover is enabled.'
}

export function buildCronCreatePrompt(durableEnabled: boolean): string {
  const durabilitySection = durableEnabled
    ? `## Durability

By default (durable: false) the job lives only in this CrabCode session — it is held in process memory, and the job is gone when CrabCode exits. Pass durable: true to hand the job to the cron daemon, which persists it so it survives restarts. Only use durable: true when the user explicitly asks for the task to persist ("keep doing this every day", "set this up permanently"). Most "remind me in 5 minutes" / "check back in an hour" requests should stay session-only.`
    : `## Experimental session-only boundary

Jobs live only in this CrabCode session — they are held in process memory, and the job is gone when CrabCode exits. Durable scheduling is unavailable while the PR-5 live-consumer gate is off. Do not call this tool with \`durable:true\` or \`at\`; explain the unsupported boundary instead.`

  const durableRuntimeNote = durableEnabled
    ? 'Durable jobs use the cron daemon occurrence ledger. Each occurrence must be claimed by its immutable execution target and written to a durable inbox before acceptance; external side effects still require idempotent handling and must not be described as exactly-once. Session-only jobs die with the process. '
    : ''

  const oneShotSection = durableEnabled
    ? `## One-shot tasks: prefer \`at\` over \`cron\`

For "remind me at X" / "5 minutes from now, do Y" / any absolute future time, use the
**\`at\`** field with an ISO-8601 timestamp — the daemon schedules it directly via
\`kind: "at"\` (no cron-expression jitter rules applied; deleteAfterRun is implicit).
\`at\` is mutually exclusive with \`cron\` and implies durable:true.

  "remind me in 5 minutes to check the deploy" → at: "<now+5min ISO>"
  "at 2:30pm today, ping me about lunch" → at: "<today 14:30 local ISO>"

Use \`cron\` with recurring:false only when the user pinned wall-clock fields and
you want jitter behavior (e.g. "tomorrow at 9am" → cron "30 9 <dom> <mon> *",
recurring:false; the scheduler nudges :00/:30 minutes to spread fleet load):
  "tomorrow morning, run the smoke test" → cron: "57 8 <tomorrow_dom> <tomorrow_month> *", recurring: false`
    : `## One-shot tasks while durable scheduling is disabled

Do not use \`at\`: it implies durable execution and is rejected by the same
live-consumer gate. A session-only one-shot may use \`cron\` with
\`recurring:false\` only when that weaker, restart-ephemeral behavior matches
the user's request. If the user asked for persistence, explain the unsupported
boundary instead of silently substituting session-only work.`

  return `Schedule a prompt to be enqueued at a future time. Use for both recurring schedules and one-shot reminders.

Uses standard 5-field cron in the user's local timezone: minute hour day-of-month month day-of-week. "0 9 * * *" means 9am local — no timezone conversion needed.

${oneShotSection}

## Recurring jobs (recurring: true, the default)

For "every N minutes" / "every hour" / "weekdays at 9am" requests:
  "*/5 * * * *" (every 5 min), "0 * * * *" (hourly), "0 9 * * 1-5" (weekdays at 9am local)

## Avoid the :00 and :30 minute marks when the task allows it

Every user who asks for "9am" gets \`0 9\`, and every user who asks for "hourly" gets \`0 *\` — which means requests from across the planet land on the API at the same instant. When the user's request is approximate, pick a minute that is NOT 0 or 30:
  "every morning around 9" → "57 8 * * *" or "3 9 * * *" (not "0 9 * * *")
  "hourly" → "7 * * * *" (not "0 * * * *")
  "in an hour or so, remind me to..." → pick whatever minute you land on, don't round

Only use minute 0 or 30 when the user names that exact time and clearly means it ("at 9:00 sharp", "at half past", coordinating with a meeting). When in doubt, nudge a few minutes early or late — the user will not notice, and the fleet will.

${durabilitySection}

## Runtime behavior

Session-only jobs only fire while the owning process is alive and the REPL is idle (not mid-query); their notifications are best-effort, so do not promise they survive a restart or can never be missed. ${durableRuntimeNote}The scheduler adds a small deterministic jitter on top of whatever you pick: recurring tasks fire up to 10% of their period late (max 15 min); one-shot tasks landing on :00 or :30 fire up to 90 s early. Picking an off-minute is still the bigger lever.

Recurring tasks auto-expire after ${DEFAULT_MAX_AGE_DAYS} days — they fire one final time, then are deleted. This bounds session lifetime. Tell the user about the ${DEFAULT_MAX_AGE_DAYS}-day limit when scheduling recurring jobs.

Returns a job ID you can pass to ${CRON_DELETE_TOOL_NAME}.`
}

export const CRON_DELETE_DESCRIPTION = 'Cancel a scheduled cron job by ID'
export function buildCronDeletePrompt(durableEnabled: boolean): string {
  return durableEnabled
    ? `Cancel a cron job previously scheduled with ${CRON_CREATE_TOOL_NAME}. Removes it from the cron daemon (durable jobs) or the in-memory session store (session-only jobs).`
    : `Cancel a cron job previously scheduled with ${CRON_CREATE_TOOL_NAME}. Removes it from the in-memory session store.`
}

export const CRON_LIST_DESCRIPTION = 'List scheduled cron jobs'
export function buildCronListPrompt(durableEnabled: boolean): string {
  return durableEnabled
    ? `List all cron jobs scheduled via ${CRON_CREATE_TOOL_NAME}, both durable (persisted by the cron daemon) and session-only.`
    : `List all cron jobs scheduled via ${CRON_CREATE_TOOL_NAME} in this session.`
}
