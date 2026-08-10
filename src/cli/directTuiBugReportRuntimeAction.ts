import { z } from 'zod/v4'

export const DIRECT_TUI_BUG_REPORT_ENDPOINT = '/crabcode_cli_feedback'

const SafeDescriptionSchema = z
  .string()
  .min(1)
  .max(8_000)
  .refine(value => new TextEncoder().encode(value).length <= 8_000)
  .refine(value => !/[\u0000-\u0008\u000b\u000c\u000e-\u001f\u007f]/.test(value))

const FeedbackIdSchema = z
  .string()
  .min(1)
  .max(160)
  .refine(
    value =>
      !/[\u0000-\u001f\u007f-\u009f\u061c\u200e\u200f\u202a-\u202e\u2066-\u2069]/.test(
        value,
      ),
  )

export const DirectTuiBugReportActionSchema = z
  .object({
    kind: z.literal('bug_report_submit'),
    description: SafeDescriptionSchema,
  })
  .strict()

const DirectTuiBugReportSubmittedResultSchema = z
  .object({
    kind: z.literal('bug_report_submitted'),
    feedback_id: FeedbackIdSchema,
  })
  .strict()

const DirectTuiBugReportUnconfirmedResultSchema = z
  .object({
    kind: z.literal('bug_report_unconfirmed'),
  })
  .strict()

export const DirectTuiBugReportResultSchema = z.union([
  DirectTuiBugReportSubmittedResultSchema,
  DirectTuiBugReportUnconfirmedResultSchema,
])

export type DirectTuiBugReportAction = z.infer<
  typeof DirectTuiBugReportActionSchema
>
export type DirectTuiBugReportResult = z.infer<
  typeof DirectTuiBugReportResultSchema
>

export type DirectTuiBugReportDependencies = {
  submitBugReport?: (description: string) => Promise<{ feedbackId: string }>
}

export interface DirectTuiBugReportClient {
  doJSON<T>(
    method: string,
    path: string,
    body: unknown | null,
    signal?: AbortSignal,
  ): Promise<T>
}

export type DirectTuiBugReportEnvironment = {
  platform: string
  terminal: string
  version: string
  datetime: string
}

export class DirectTuiBugReportUnconfirmedError extends Error {
  constructor() {
    super('bug-report-response-missing-feedback-id')
    this.name = 'DirectTuiBugReportUnconfirmedError'
  }
}

type RawBugReportResponse =
  | {
      feedback_id?: unknown
      data?: { feedback_id?: unknown } | null
    }
  | undefined

export async function handleDirectTuiBugReportAction(
  action: DirectTuiBugReportAction,
  dependencies: DirectTuiBugReportDependencies = {},
): Promise<DirectTuiBugReportResult> {
  const submit =
    dependencies.submitBugReport ??
    (async (description: string) => {
      const [{ getAcosmiClient }, { resolveMacroVersion }] = await Promise.all([
        import('../services/acosmi/index.js'),
        import('../utils/macroVersion.js'),
      ])
      return submitDirectTuiBugReport(await getAcosmiClient(), description, {
        platform: process.platform,
        terminal: process.env.TERM_PROGRAM ?? process.env.TERM ?? 'unknown',
        version: resolveMacroVersion(),
        datetime: new Date().toISOString(),
      })
    })

  try {
    const result = await submit(action.description)
    return DirectTuiBugReportResultSchema.parse({
      kind: 'bug_report_submitted',
      feedback_id: result.feedbackId,
    })
  } catch (error) {
    if (error instanceof DirectTuiBugReportUnconfirmedError) {
      return { kind: 'bug_report_unconfirmed' }
    }
    throw error
  }
}

/**
 * Submit the smallest useful report: the user-authored description and basic
 * build environment only. Conversation text, request bodies, and logs are not
 * read by this path.
 */
export async function submitDirectTuiBugReport(
  client: DirectTuiBugReportClient,
  description: string,
  environment: DirectTuiBugReportEnvironment,
): Promise<{ feedbackId: string }> {
  const safeDescription = SafeDescriptionSchema.parse(description)
  const raw = await client.doJSON<RawBugReportResponse>(
    'POST',
    DIRECT_TUI_BUG_REPORT_ENDPOINT,
    {
      content: JSON.stringify({
        description: safeDescription,
        platform: safeEnvironmentField(environment.platform),
        terminal: safeEnvironmentField(environment.terminal),
        version: safeEnvironmentField(environment.version),
        datetime: safeEnvironmentField(environment.datetime),
      }),
    },
  )
  const feedbackId =
    FeedbackIdSchema.safeParse(raw?.feedback_id).data ??
    FeedbackIdSchema.safeParse(raw?.data?.feedback_id).data
  if (feedbackId === undefined) {
    throw new DirectTuiBugReportUnconfirmedError()
  }
  return { feedbackId }
}

function safeEnvironmentField(value: string): string {
  return value
    .replace(/[\u0000-\u001f\u007f-\u009f\u202a-\u202e\u2066-\u2069]/g, ' ')
    .replace(/\s+/g, ' ')
    .trim()
    .slice(0, 160)
}
