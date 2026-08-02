// W-GOAL-LIFECYCLE PR-1 (2026-06-13) — terminal/phase event source for the
// /goal acceptance loop. Goal completion was previously a pure model behavior
// (RC-4): the `activeGoal` AppState was sticky with zero clear points, and the
// goal console (W-GOAL-CONSOLE) had no structured data source. GoalReport is
// the lightweight tool the goal skill calls at phase transitions and at the
// terminal verdict so the TUI can capture a code-observable event (clear the
// status line, fire an OS notification, feed the console).
//
// Direct interactive-session contract: QueryEngine and the tool share the same
// TypeScript AppState authority. The tool applies the lifecycle transition
// there and returns the established structured tool_result for presentation.
// Non-interactive SDK/headless calls retain the prior echo-only behavior. The
// renderer remains a consumer; no public schema or transport method is added.
export const GOAL_REPORT_TOOL_NAME = 'GoalReport'

/** Terminal verdicts. `present verdict` marks a GoalReport as TERMINAL — the
 * goal loop has ended. Absent verdict (phase report) is a progress signal for
 * the console (W-GOAL-CONSOLE) and never clears the goal. */
export const GOAL_REPORT_VERDICTS = [
  'pass',
  'fail',
  'partial',
  'blocked',
] as const
export type GoalReportVerdict = (typeof GOAL_REPORT_VERDICTS)[number]
