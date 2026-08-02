import type { AppState } from '../../state/AppStateStore.js'
import type { GoalReportVerdict } from './constants.js'

export type GoalReportLifecycleEvent = {
  terminal: boolean
  summary: string
  verdict?: GoalReportVerdict
  phase?: string
  reportPath?: string
}

export type GoalReportLifecycleTransition = {
  state: AppState
  terminalTransition: boolean
}

export type AcceptedGoalSlashCommand = {
  commandName: string
  parsedArgs: string
  emittedMessageCount: number
  shouldQuery: boolean
  startedAt: number
}

/**
 * Derive the exact a81 `/goal` start transition after slash-command
 * expansion. Unknown/rejected/argless/local-only commands return null so the
 * caller does not touch AppState.
 */
export function deriveAcceptedGoalStart(
  input: AcceptedGoalSlashCommand,
): NonNullable<AppState['activeGoal']> | null {
  const goalText = input.parsedArgs.trim()
  if (
    input.commandName !== 'goal' ||
    goalText.length === 0 ||
    input.emittedMessageCount === 0 ||
    !input.shouldQuery
  ) {
    return null
  }
  return {
    text: goalText,
    startedAt: input.startedAt,
  }
}

/**
 * Retire the terminal GoalStatusLine on the next ordinary direct-TUI turn.
 *
 * A task-notification continuation is not a user-visible acknowledgement of
 * the terminal result, so it must preserve the completed goal. Running goals
 * and already-empty state retain reference identity.
 */
export function clearCompletedGoalOnNextInput(
  prev: AppState,
  isTaskNotificationTurn: boolean,
): AppState {
  if (
    isTaskNotificationTurn ||
    prev.activeGoal?.completedAt === undefined
  ) {
    return prev
  }
  return { ...prev, activeGoal: undefined }
}

/**
 * Apply one GoalReport in the same TypeScript session that owns AppState.
 *
 * This is a session-state reducer, not a renderer or transport adapter.
 * Terminal notifications are emitted by the caller only when
 * `terminalTransition` is true.
 */
export function applyGoalReportToSessionState(
  prev: AppState,
  report: GoalReportLifecycleEvent,
  reportedAt: number = Date.now(),
): GoalReportLifecycleTransition {
  if (report.terminal) {
    const goal = prev.activeGoal
    if (!goal || goal.completedAt !== undefined) {
      return { state: prev, terminalTransition: false }
    }
    return {
      state: {
        ...prev,
        activeGoal: {
          ...goal,
          completedAt: reportedAt,
          ...(report.verdict !== undefined
            ? { verdict: report.verdict }
            : {}),
          summary: report.summary,
        },
      },
      terminalTransition: true,
    }
  }

  const goal = prev.activeGoal
  if (!goal || goal.completedAt !== undefined) {
    return { state: prev, terminalTransition: false }
  }
  if (goal.phase === report.phase && goal.phaseDetail === report.summary) {
    return { state: prev, terminalTransition: false }
  }

  const phaseHistory = updatePhaseHistory(
    goal.phaseHistory,
    report,
    reportedAt,
  )
  return {
    state: {
      ...prev,
      activeGoal: {
        ...goal,
        ...(report.phase !== undefined ? { phase: report.phase } : {}),
        phaseDetail: report.summary,
        ...(phaseHistory !== undefined ? { phaseHistory } : {}),
      },
    },
    terminalTransition: false,
  }
}

function updatePhaseHistory(
  previous: NonNullable<AppState['activeGoal']>['phaseHistory'],
  report: GoalReportLifecycleEvent,
  reportedAt: number,
):
  | ReadonlyArray<{ phase: string; detail?: string; reportedAt: number }>
  | undefined {
  const phase = report.phase
  if (phase === undefined) return undefined

  const history = previous ?? []
  const phaseKey = normalizePhase(phase)
  const existingIndex = history.findIndex(
    entry => normalizePhase(entry.phase) === phaseKey,
  )
  if (existingIndex === -1) {
    return [
      ...history,
      { phase, detail: report.summary, reportedAt },
    ]
  }

  const existing = history[existingIndex]
  if (!existing || existing.detail === report.summary) return history
  const next = history.slice()
  next[existingIndex] = {
    ...existing,
    phase,
    detail: report.summary,
  }
  return next
}

function normalizePhase(phase: string): string {
  return phase.toLowerCase().trim()
}
