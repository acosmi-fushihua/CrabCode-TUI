import { z } from 'zod/v4'
import { buildTool, type ToolDef } from '../../Tool.js'
import { cronToHuman } from '../../utils/cron.js'
import { listAllCronTasks } from '../../utils/cronTasks.js'
import { truncate } from '../../utils/format.js'
import { lazySchema } from '../../utils/lazySchema.js'
import { getTeammateContext } from '../../utils/teammateContext.js'
import {
  buildCronListPrompt,
  CRON_LIST_DESCRIPTION,
  CRON_LIST_TOOL_NAME,
  isDurableCronEnabled,
  isKairosCronEnabled,
} from './prompt.js'
import { createToolPresentationDelegates } from '../toolPresentationRegistry.js'

const inputSchema = lazySchema(() => z.strictObject({}))
type InputSchema = ReturnType<typeof inputSchema>

/**
 * Render an `at` job's ISO-8601 fire time as a readable local-time string.
 * Mirrors CronCreateTool's `at ${new Date(at).toLocaleString()}` output so
 * the list and the create result agree. Falls back to the raw ISO string if
 * the timestamp is unparseable (defensive — never throw from a list call).
 */
export function formatAtSchedule(iso: string): string {
  const ms = Date.parse(iso)
  if (Number.isNaN(ms)) return `at ${iso}`
  return `at ${new Date(ms).toLocaleString()}`
}

const outputSchema = lazySchema(() =>
  z.object({
    jobs: z.array(
      z.object({
        id: z.string(),
        /** Empty string for `at` jobs (no cron expression). */
        cron: z.string(),
        /** Native schedule kind. Absent ⇒ treat as 'cron' (back-compat). */
        scheduleKind: z.enum(['cron', 'at', 'every']).optional(),
        /** ISO-8601 absolute fire time — set only for `at` jobs. */
        at: z.string().optional(),
        everyMs: z.number().int().optional(),
        humanSchedule: z.string(),
        prompt: z.string(),
        recurring: z.boolean().optional(),
        durable: z.boolean().optional(),
      }),
    ),
  }),
)
type OutputSchema = ReturnType<typeof outputSchema>
export type ListOutput = z.infer<OutputSchema>

export const CronListTool = buildTool({
  name: CRON_LIST_TOOL_NAME,
  searchHint: 'list active cron jobs',
  maxResultSizeChars: 100_000,
  shouldDefer: true,
  get inputSchema(): InputSchema {
    return inputSchema()
  },
  get outputSchema(): OutputSchema {
    return outputSchema()
  },
  isEnabled() {
    return isKairosCronEnabled()
  },
  isConcurrencySafe() {
    return true
  },
  isReadOnly() {
    return true
  },
  async description() {
    return CRON_LIST_DESCRIPTION
  },
  async prompt() {
    return buildCronListPrompt(isDurableCronEnabled())
  },
  async call() {
    const allTasks = await listAllCronTasks()
    // Teammates only see their own crons; team lead (no ctx) sees all.
    const ctx = getTeammateContext()
    const tasks = ctx
      ? allTasks.filter(t => t.agentId === ctx.agentId)
      : allTasks
    const jobs = tasks.map(t => {
      // `at` jobs carry no cron expression — render the absolute local time
      // instead of running cronToHuman('') (which would be meaningless).
      const isAt = t.scheduleKind === 'at'
      const isEvery = t.scheduleKind === 'every'
      return {
        id: t.id,
        cron: t.cron,
        ...(t.scheduleKind ? { scheduleKind: t.scheduleKind } : {}),
        ...(isAt && t.at ? { at: t.at } : {}),
        ...(isEvery && t.everyMs !== undefined ? { everyMs: t.everyMs } : {}),
        humanSchedule:
          isAt && t.at
            ? formatAtSchedule(t.at)
            : isEvery && t.everyMs !== undefined
              ? `every ${t.everyMs / 1000} seconds (fixed anchor)`
              : cronToHuman(t.cron),
        prompt: t.prompt,
        ...(t.recurring ? { recurring: true } : {}),
        ...(t.durable === false ? { durable: false } : {}),
      }
    })
    return { data: { jobs } }
  },
  mapToolResultToToolResultBlockParam(output, toolUseID) {
    return {
      tool_use_id: toolUseID,
      type: 'tool_result',
      content:
        output.jobs.length > 0
          ? output.jobs
              .map(j => {
                // `at` jobs are always one-shot (P1: ScheduleKind::At). The
                // `humanSchedule` for an `at` job is already "at <time>", so
                // the suffix is just the one-shot label.
                const label =
                  j.scheduleKind === 'at' || !j.recurring
                    ? ' (one-shot)'
                    : ' (recurring)'
                const scope = j.durable === false ? ' [session-only]' : ''
                return `${j.id} — ${j.humanSchedule}${label}${scope}: ${truncate(j.prompt, 80, true)}`
              })
              .join('\n')
          : 'No scheduled jobs.',
    }
  },
  ...createToolPresentationDelegates(CRON_LIST_TOOL_NAME, [
    'renderToolUseMessage',
    'renderToolResultMessage',
  ]),
} satisfies ToolDef<InputSchema, ListOutput>)
