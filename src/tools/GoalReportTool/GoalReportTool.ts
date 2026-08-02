import { z } from 'zod/v4'
import { buildTool, type ToolDef } from '../../Tool.js'
import { lazySchema } from '../../utils/lazySchema.js'
import {
  GOAL_REPORT_TOOL_NAME,
  GOAL_REPORT_VERDICTS,
} from './constants.js'
import { DESCRIPTION, getPrompt } from './prompt.js'
import {
  applyGoalReportToSessionState,
  type GoalReportLifecycleEvent,
} from './sessionLifecycle.js'

const inputSchema = lazySchema(() =>
  z.strictObject({
    summary: z
      .string()
      .describe('One human-readable line describing the current state/outcome'),
    verdict: z
      .enum(GOAL_REPORT_VERDICTS)
      .optional()
      .describe(
        'Presence marks the TERMINAL report (loop ended). One of pass | fail | partial | blocked. Omit for phase/progress reports.',
      ),
    phase: z
      .string()
      .optional()
      .describe(
        'Phase label for a progress report (criteria | implement | verify | spotcheck, or short custom). Omit on the terminal report.',
      ),
    reportPath: z
      .string()
      .optional()
      .describe('Repo-relative path to a written report/audit doc, if any'),
  }),
)
type InputSchema = ReturnType<typeof inputSchema>

const outputSchema = lazySchema(() =>
  z.object({
    // Echo the same structured fields into the established tool-result event.
    // `terminal` is derived from verdict presence; no transport field or
    // renderer-specific envelope is introduced here.
    terminal: z.boolean(),
    summary: z.string(),
    verdict: z.enum(GOAL_REPORT_VERDICTS).optional(),
    phase: z.string().optional(),
    reportPath: z.string().optional(),
  }),
)
type OutputSchema = ReturnType<typeof outputSchema>

export type Output = z.infer<OutputSchema> & GoalReportLifecycleEvent

export const GoalReportTool = buildTool({
  name: GOAL_REPORT_TOOL_NAME,
  searchHint: 'report /goal lifecycle phase or terminal verdict',
  maxResultSizeChars: 16_000,
  async description() {
    return DESCRIPTION
  },
  async prompt() {
    return getPrompt()
  },
  get inputSchema(): InputSchema {
    return inputSchema()
  },
  get outputSchema(): OutputSchema {
    return outputSchema()
  },
  userFacingName() {
    return GOAL_REPORT_TOOL_NAME
  },
  // Always available so the goal skill can call it. Spurious calls are
  // session-state no-ops when no activeGoal exists. It is not deferred so the
  // model reliably sees it during a goal session.
  isEnabled() {
    return true
  },
  isConcurrencySafe() {
    // The reducer runs synchronously before this async call returns and is
    // idempotent, so it preserves the tool's established concurrency contract.
    return true
  },
  toAutoClassifierInput(input) {
    return input.summary
  },
  renderToolUseMessage() {
    return null
  },
  async call({ summary, verdict, phase, reportPath }, context) {
    const terminal = verdict !== undefined
    const data: Output = {
      terminal,
      summary,
      ...(verdict !== undefined ? { verdict } : {}),
      ...(phase !== undefined ? { phase } : {}),
      ...(reportPath !== undefined ? { reportPath } : {}),
    }

    let terminalTransition = false
    if (!context.options.isNonInteractiveSession) {
      context.setAppState(prev => {
        const transition = applyGoalReportToSessionState(prev, data)
        terminalTransition ||= transition.terminalTransition
        return transition.state
      })
    }
    if (terminalTransition) {
      const verdictLabel = verdict?.toUpperCase() ?? 'DONE'
      const pathSuffix = reportPath ? ` · ${reportPath}` : ''
      context.sendOSNotification?.({
        message: `Goal ${verdictLabel}: ${summary}${pathSuffix}`,
        notificationType: 'goal_complete',
      })
    }

    return { data }
  },
  mapToolResultToToolResultBlockParam(content, toolUseID) {
    const { terminal, verdict, phase, summary } = content as Output
    const label = terminal
      ? `Goal terminal report recorded (verdict: ${verdict ?? 'unknown'})`
      : `Goal phase report recorded${phase ? ` (${phase})` : ''}`
    return {
      tool_use_id: toolUseID,
      type: 'tool_result',
      content: `${label}: ${summary}`,
    }
  },
} satisfies ToolDef<InputSchema, Output>)
