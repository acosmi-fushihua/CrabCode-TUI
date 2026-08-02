import { GOAL_REPORT_TOOL_NAME } from './constants.js'

export const DESCRIPTION = `Report the lifecycle state of an active /goal session.

Call this tool ONLY while running a /goal acceptance loop:

- At each phase transition (criteria defined → implementing → verifying →
  spot-checking), call it with a \`phase\` and a short \`status\` so the goal
  console reflects real progress instead of a frozen status line.
- At the TERMINAL point of the loop — a spot-checked VERDICT: PASS, an honest
  PARTIAL, or a hard stop (blocked / impossible) — call it with a \`verdict\`
  and a one-line \`summary\`. The terminal report is what closes the goal:
  it clears the goal status line and notifies the user the goal is done.

Do not call this tool outside a /goal session — it is a no-op there.`

export function getPrompt(): string {
  return `${GOAL_REPORT_TOOL_NAME} records goal-loop lifecycle events so the TUI can show real phase progress and detect terminal completion.

Fields:
- \`summary\` (required): one human-readable line. For a phase report, what is
  happening now ("verifying 5 criteria with the adversarial agent"). For a
  terminal report, the outcome ("All 6 acceptance criteria pass, spot-checked").
- \`verdict\` (optional): one of pass | fail | partial | blocked. Presence of
  \`verdict\` marks this as the TERMINAL report — call it exactly ONCE, when the
  loop has genuinely ended (spot-checked PASS, honest PARTIAL, or a hard stop).
- \`phase\` (optional): the current phase label for progress reports
  (criteria | implement | verify | spotcheck, or a short custom label). Omit on
  the terminal report.
- \`reportPath\` (optional): repo-relative path to a written report/audit doc,
  if one was produced.

Rules:
- The terminal report (with \`verdict\`) is mandatory before you end a /goal
  session — it is the only signal that clears the goal and notifies the user.
- Never fabricate a PASS: only report \`verdict: 'pass'\` after a spot-checked
  PASS exactly as the /goal completion criteria require.
- Phase reports are optional progress signals; the terminal report is not.`
}
