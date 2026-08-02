// /goal combines the coordinator QA workflow
// (src/coordinator/coordinatorMode.ts) and the built-in adversarial
// verification agent (src/tools/AgentTool/built-in/verificationAgent.ts).
// /goal wires the main-loop session to that verification agent: define
// acceptance criteria up front, implement, verify adversarially, loop
// until VERDICT: PASS.
import {
  AGENT_TOOL_NAME,
  VERIFICATION_AGENT_TYPE,
} from '../../tools/AgentTool/constants.js'
import { GOAL_REPORT_TOOL_NAME } from '../../tools/GoalReportTool/constants.js'
import { isEnvTruthy } from '../../utils/envUtils.js'
import { feature } from '../../utils/featurePolyfill.js'
import { registerBundledSkill } from '../bundledSkills.js'

const USAGE_MESSAGE = `Usage: /goal <goal description>

Run a goal-completion loop: define verifiable acceptance criteria, implement,
adversarially verify with an independent verification agent, and repeat until
every criterion passes. Completion is only reported after the verifier issues
VERDICT: PASS.

Examples:
  /goal fix the flaky auth test and make the full suite pass
  /goal add dark mode to settings, persisted across restarts`

function buildPrompt(args: string): string {
  return `# /goal — drive this goal to verified completion

The user has handed over a goal. You do not report completion until an
independent adversarial verifier has issued VERDICT: PASS against explicit
acceptance criteria. "Done" is defined before work starts, not after.

## Phase 1 — Define acceptance criteria

Derive 3-7 concrete, independently checkable criteria from the goal. Each
criterion must name the command/observable that proves it — a command to run,
an endpoint to hit, a file state to inspect. Include baseline criteria when
applicable: build passes, full test suite passes, type checks/linters pass.

State the criteria to the user in your first reply BEFORE starting
implementation. Criteria are a contract: you may ADD criteria as you learn,
but you must never weaken, drop, or reinterpret one to make passing easier —
only the user can relax a criterion.

## Phase 2 — Implement

Plan and execute with your normal tools. For multi-step work, track progress
with the task list. Fix root causes, not symptoms. Do not pre-announce
success.

## Phase 3 — Adversarial verification gate

When you believe every criterion is met, spawn the ${AGENT_TOOL_NAME} tool
with subagent_type "${VERIFICATION_AGENT_TYPE}". This agent always runs in
the background — its report arrives later as a task-notification message, or
you may retrieve it explicitly with the TaskOutput tool (block: true); both
paths are equivalent and safe. You MUST wait for that verdict; do not report
completion (or start a victory summary) while it is pending.

Pass it: the original goal verbatim, the full acceptance criteria list, every
file changed, the approach taken, and the exact build/test commands for this
project. Do not share your own test results or claim things work — let it
find out.

## Phase 4 — Verdict handling

- **VERDICT: FAIL** — fix the root cause of each finding, then resume the
  SAME verifier (SendMessage with its agentId) with its findings plus what
  you changed; repeat the gate. Never argue a finding away without evidence.
- **VERDICT: PASS** — spot-check it: re-run 2-3 commands from its report and
  confirm each PASS has a command-output block matching your re-run. Any
  mismatch or evidence-free PASS → resume the verifier with the specifics.
  Only after a spot-checked PASS do you report completion.
- **VERDICT: PARTIAL** — allowed only for environmental limitations. Report
  honestly what passed, what could not be verified and why. PARTIAL is not
  PASS — say so plainly.

## Reporting lifecycle state (required)

The TUI shows a live goal status line and console driven by the
${GOAL_REPORT_TOOL_NAME} tool — call it to keep them truthful:

- **At the START of each phase**, call ${GOAL_REPORT_TOOL_NAME} with a \`phase\`
  (use the canonical labels \`criteria\` → \`implement\` → \`verify\` →
  \`spotcheck\`) and a one-line \`summary\` of what that phase is doing. This
  drives the goal status line / console so progress is visible instead of a
  frozen line. These phase reports carry NO \`verdict\` (they are not terminal).
- **At the terminal point — REQUIRED, exactly once** — when the loop genuinely
  ends, call ${GOAL_REPORT_TOOL_NAME} with a \`verdict\` (\`pass\` after a
  spot-checked PASS · \`partial\` for honest PARTIAL · \`fail\` or \`blocked\`
  for a hard stop per the Stop conditions) and a one-line \`summary\` (and
  \`reportPath\` if you wrote a report). This terminal call is the ONLY signal
  that closes the goal status line and notifies the user; do not end a /goal
  session without it. Never report \`verdict: 'pass'\` without a spot-checked
  PASS.

## Stop conditions

This loop has no turn budget — keep going until PASS. The only honest exits:

(a) a spot-checked PASS;
(b) 3 consecutive FAILs on the same criterion with no new information — stop,
    report the criterion, the attempts, and the exact blocking evidence, and
    ask the user how to proceed;
(c) the goal turns out to be impossible or contradictory — report why, with
    evidence.

Never exit by silently shrinking the goal.

## Completion report

A checklist of every acceptance criterion with the evidence (command + key
output) and the final verdict line quoted.

## The goal

${args}`
}

export function registerGoalSkill(): void {
  registerBundledSkill({
    name: 'goal',
    description:
      'Drive a goal to verified completion: define acceptance criteria, implement, adversarially verify, repeat until everything passes',
    whenToUse:
      'When the user wants an outcome guaranteed against explicit acceptance criteria (e.g. "make X work and don\'t stop until tests pass", "不达标不收工"). Do NOT invoke for quick one-off edits or questions.',
    argumentHint: '<goal description>',
    userInvocable: true,
    // Coordinator sessions replace the built-in agent catalog with
    // worker-only (builtInAgents.ts → getCoordinatorAgents), so
    // subagent_type 'verification' is unresolvable there and the
    // coordinator prompt carries its own QA workflow.
    isEnabled: () =>
      !(
        feature('COORDINATOR_MODE') &&
        isEnvTruthy(process.env.CRABCODE_COORDINATOR_MODE)
      ),
    async getPromptForCommand(args, context) {
      const trimmed = args.trim()
      if (!trimmed) {
        // W-GOAL-LIFECYCLE PR-2: argless /goal reports the current/last goal's
        // status (previously only printed usage).
        const goal = context.getAppState().activeGoal
        const status = goal
          ? goal.completedAt !== undefined
            ? `\n\nLast goal: ${goal.text}\nStatus: completed${
                goal.verdict ? ` (verdict ${goal.verdict.toUpperCase()})` : ''
              }${goal.summary ? `\n  ${goal.summary}` : ''}`
            : `\n\nActive goal: ${goal.text}\nStatus: running (no terminal verdict yet)`
          : '\n\nNo active goal in this session.'
        return [{ type: 'text', text: `${USAGE_MESSAGE}${status}` }]
      }
      return [{ type: 'text', text: buildPrompt(trimmed) }]
    },
  })
}
