import { z } from 'zod/v4'
import { setScheduledTasksEnabled } from '../../bootstrap/state.js'
import type { ValidationResult } from '../../Tool.js'
import { buildTool, type ToolDef } from '../../Tool.js'
import { cronToHuman, parseCronExpression } from '../../utils/cron.js'
import {
  addCronTask,
  listAllCronTasks,
  nextCronRunMs,
} from '../../utils/cronTasks.js'
import { lazySchema } from '../../utils/lazySchema.js'
import { semanticBoolean } from '../../utils/semanticBoolean.js'
import { getTeammateContext } from '../../utils/teammateContext.js'
import {
  buildCronCreateDescription,
  buildCronCreatePrompt,
  CRON_CREATE_TOOL_NAME,
  DEFAULT_MAX_AGE_DAYS,
  isDurableCronEnabled,
  isKairosCronEnabled,
} from './prompt.js'
import { createToolPresentationDelegates } from '../toolPresentationRegistry.js'

const MAX_JOBS = 50

const inputSchema = lazySchema(() =>
  z
    .strictObject({
      cron: z
        .string()
        .optional()
        .describe(
          'Standard 5-field cron expression in local time. Mutually exclusive with `at` and `everyMs`; exactly one schedule must be set.',
        ),
      at: z
        .string()
        .datetime({ offset: true })
        .optional()
        .describe(
          'ISO-8601 absolute fire time (must be in the future). One-shot. Mutually exclusive with `cron`/`everyMs`. Implies durable:true.',
        ),
      everyMs: z
        .number()
        .int()
        .min(1000)
        .max(30 * 24 * 60 * 60 * 1000)
        .optional()
        .describe(
          'Native fixed interval in milliseconds (>=1000). Mutually exclusive with cron/at; anchored at creation time and never rounded to cron.',
        ),
      prompt: z
        .string()
        .trim()
        .min(1, 'prompt must contain non-whitespace text')
        .describe('The prompt to enqueue at each fire time.'),
      recurring: semanticBoolean(z.boolean().optional()).describe(
        `true (default) = fire on every cron match until deleted or auto-expired after ${DEFAULT_MAX_AGE_DAYS} days. false = fire once at the next match, then auto-delete. Ignored when \`at\` is set (always one-shot). Use false for "remind me at X" one-shot requests with pinned minute/hour/dom/month.`,
      ),
      durable: semanticBoolean(z.boolean().optional()).describe(
        'true = persist through the cron daemon occurrence ledger and survive restarts (currently release-gated off). false (default) = in-memory only, dies when this CrabCode session ends. Use true only when the user asks the task to survive across sessions. Forced true when `at` is set.',
      ),
      sessionTarget: z
        .enum(['continuation', 'main', 'isolated'])
        .optional()
        .describe(
          'continuation (default) routes to the exact creating thread; main routes to its lead. isolated is currently unsupported and is rejected.',
        ),
      wakeMode: z
        .enum(['now', 'next-heartbeat'])
        .optional()
        .describe(
          'next-heartbeat (default) waits for a clean scheduler tick. now requests an immediate wake in the direct TUI session.',
        ),
    })
    .refine(
      d => [Boolean(d.cron), Boolean(d.at), d.everyMs !== undefined].filter(Boolean).length === 1,
      {
      message: 'exactly one of `cron`, `at`, or `everyMs` must be provided',
      path: ['cron'],
      },
    ),
)
type InputSchema = ReturnType<typeof inputSchema>

const outputSchema = lazySchema(() =>
  z.object({
    id: z.string(),
    humanSchedule: z.string(),
    recurring: z.boolean(),
    durable: z.boolean().optional(),
    sessionTarget: z.enum(['continuation', 'main']),
    wakeMode: z.enum(['now', 'next-heartbeat']),
    nextRunAtMs: z.number().int().optional(),
    expiresAtMs: z.number().int().optional(),
  }),
)
type OutputSchema = ReturnType<typeof outputSchema>
export type CreateOutput = z.infer<OutputSchema>

export const CronCreateTool = buildTool({
  name: CRON_CREATE_TOOL_NAME,
  searchHint: 'schedule a recurring or one-shot prompt',
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
  toAutoClassifierInput(input) {
    return `${input.cron ?? (input.everyMs !== undefined ? `every ${input.everyMs}ms` : `at ${input.at}`)}: ${input.prompt}`
  },
  async description() {
    return buildCronCreateDescription(isDurableCronEnabled())
  },
  async prompt() {
    return buildCronCreatePrompt(isDurableCronEnabled())
  },
  async validateInput(input): Promise<ValidationResult> {
    if (!isKairosCronEnabled()) {
      return {
        result: false,
        message:
          'local automations are experimental and disabled; CronCreate is not available in this runtime',
        errorCode: 7,
      }
    }
    if (input.sessionTarget === 'isolated') {
      return {
        result: false,
        message:
          'sessionTarget=isolated is unsupported: no isolated cron executor is implemented',
        errorCode: 7,
      }
    }
    const teammate = getTeammateContext()
    if ((input.at != null || input.durable === true) && !isDurableCronEnabled()) {
      return {
        result: false,
        message:
          'durable scheduling is unsupported while the PR-5 live-consumer gate is disabled; `at` cannot bypass this gate',
        errorCode: 7,
      }
    }
    if (input.at) {
      const fireMs = Date.parse(input.at)
      if (Number.isNaN(fireMs)) {
        return {
          result: false,
          message: `Invalid \`at\` timestamp '${input.at}'. Expected ISO-8601 (e.g. 2026-05-07T14:30:00+08:00).`,
          errorCode: 5,
        }
      }
      if (fireMs <= Date.now()) {
        return {
          result: false,
          message: `\`at\` timestamp '${input.at}' is not in the future (now=${new Date().toISOString()}).`,
          errorCode: 6,
        }
      }
      // `at` always durable; teammate gating same as cron+durable.
      if (teammate) {
        return {
          result: false,
          message:
            '`at` schedules are not supported for teammates (teammates do not persist across sessions)',
          errorCode: 4,
        }
      }
    } else if (input.cron) {
      if (!parseCronExpression(input.cron)) {
        return {
          result: false,
          message: `Invalid cron expression '${input.cron}'. Expected 5 fields: M H DoM Mon DoW.`,
          errorCode: 1,
        }
      }
      if (nextCronRunMs(input.cron, Date.now()) === null) {
        return {
          result: false,
          message: `Cron expression '${input.cron}' does not match any calendar date in the next year.`,
          errorCode: 2,
        }
      }
      // Teammates don't persist across sessions, so a durable teammate cron
      // would orphan on restart (agentId would point to a nonexistent teammate).
      if (input.durable && teammate) {
        return {
          result: false,
          message:
            'durable crons are not supported for teammates (teammates do not persist across sessions)',
          errorCode: 4,
        }
      }
    } else if (input.everyMs !== undefined && teammate && input.durable) {
      return {
        result: false,
        message:
          'durable every schedules are not supported for teammates (teammates do not persist across sessions)',
        errorCode: 4,
      }
    }
    const tasks = await listAllCronTasks()
    if (tasks.length >= MAX_JOBS) {
      return {
        result: false,
        message: `Too many scheduled jobs (max ${MAX_JOBS}). Cancel one first.`,
        errorCode: 3,
      }
    }
    return { result: true }
  },
  async call({
    cron,
    at,
    everyMs,
    prompt,
    recurring = true,
    durable = false,
    sessionTarget = 'continuation',
    wakeMode = 'next-heartbeat',
  }) {
    if (!isKairosCronEnabled()) {
      throw new Error(
        'unsupported schedule: local automations are experimental and disabled',
      )
    }
    if (sessionTarget === 'isolated') {
      throw new Error(
        'unsupported schedule target: isolated execution is not implemented',
      )
    }
    const teammate = getTeammateContext()
    // Re-check at execution time because a fleet kill switch can flip after
    // validateInput. Durable requests fail closed; they are never silently
    // downgraded to a different session-only contract. `at` implies durable
    // and must pass this same gate.
    if ((at != null || durable) && !isDurableCronEnabled()) {
      throw new Error(
        'unsupported durable schedule: PR-5 live-consumer gate is disabled',
      )
    }
    if (at) {
      const id = await addCronTask('', prompt, false, true, undefined, at, {
        sessionTarget,
        wakeMode,
      })
      setScheduledTasksEnabled(true)
      return {
        data: {
          id,
          humanSchedule: `at ${new Date(at).toLocaleString()}`,
          recurring: false,
          durable: true,
          sessionTarget,
          wakeMode,
          nextRunAtMs: Date.parse(at),
        },
      }
    }
    const effectiveDurable = durable
    const createdAt = Date.now()
    const effectiveRecurring = everyMs !== undefined ? true : recurring
    const expiresAtMs =
      createdAt + DEFAULT_MAX_AGE_DAYS * 24 * 60 * 60 * 1000
    const id = await addCronTask(
      cron ?? '',
      prompt,
      effectiveRecurring,
      effectiveDurable,
      teammate?.agentId,
      undefined,
      {
        ...(everyMs !== undefined ? { everyMs, anchorMs: createdAt } : {}),
        sessionTarget,
        wakeMode,
        expiresAtMs,
      },
    )
    // Enable the scheduler so the task fires in this session. Durable tasks
    // fire via the crabcode-cron daemon outbox; session-only (durable:false)
    // tasks fire via the useScheduledTasks session tick (sessionCronTick.ts)
    // — both are driven once this flag is set.
    setScheduledTasksEnabled(true)
    return {
      data: {
        id,
        humanSchedule:
          everyMs !== undefined
            ? `every ${everyMs / 1000} seconds (fixed anchor)`
            : cronToHuman(cron!),
        recurring: effectiveRecurring,
        durable: effectiveDurable,
        sessionTarget,
        wakeMode,
        nextRunAtMs:
          everyMs !== undefined ? createdAt + everyMs : nextCronRunMs(cron!, createdAt) ?? undefined,
        expiresAtMs,
      },
    }
  },
  mapToolResultToToolResultBlockParam(output, toolUseID) {
    const where = output.durable
      ? 'Persisted by the cron daemon occurrence ledger'
      : 'Session-only (not written to disk, dies when CrabCode exits)'
    const routing = ` Target=${output.sessionTarget}; wake=${output.wakeMode}.`
    const expiry = output.expiresAtMs
      ? ` Expires ${new Date(output.expiresAtMs).toLocaleString()}.`
      : ''
    return {
      tool_use_id: toolUseID,
      type: 'tool_result',
      content: output.recurring
        ? `Scheduled recurring job ${output.id} (${output.humanSchedule}). ${where}.${routing}${expiry} Use CronDelete to cancel sooner.`
        : `Scheduled one-shot task ${output.id} (${output.humanSchedule}). ${where}.${routing} It will fire once then auto-delete.`,
    }
  },
  ...createToolPresentationDelegates(CRON_CREATE_TOOL_NAME, [
    'renderToolUseMessage',
    'renderToolResultMessage',
  ]),
} satisfies ToolDef<InputSchema, CreateOutput>)
