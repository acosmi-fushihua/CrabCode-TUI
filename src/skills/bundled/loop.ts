import {
  CRON_CREATE_TOOL_NAME,
  CRON_DELETE_TOOL_NAME,
  isDurableCronEnabled,
  isKairosCronEnabled,
} from '../../tools/ScheduleCronTool/prompt.js'
import { registerBundledSkill } from '../bundledSkills.js'

const DEFAULT_INTERVAL = '10m'

export const LOOP_USAGE_MESSAGE = `Usage: /loop [interval] <prompt>

Run a prompt or slash command on a recurring interval.

Intervals: Ns, Nm, Nh, Nd (e.g. 5s, 5m, 2h, 1d). Minimum granularity is 1 second.
If no interval is specified, defaults to ${DEFAULT_INTERVAL}.

Examples:
  /loop 5m /babysit-prs
  /loop 30m check the deploy
  /loop 1h /standup 1
  /loop check the deploy          (defaults to ${DEFAULT_INTERVAL})
  /loop check the deploy every 20m`

export function buildLoopPrompt(
  args: string,
  durableEnabled: boolean,
): string {
  const durableInstruction = durableEnabled
    ? '- `durable`: `false` unless the user explicitly asked for persistence across CrabCode restarts'
    : '- `durable`: always `false`. If the user asks for persistence or an absolute-time durable task, explain that durable automation is experimental and unavailable; do not create a weaker session-only substitute'
  return `# /loop — schedule a recurring prompt

Parse the input below into \`[interval] <prompt…>\` and schedule it with ${CRON_CREATE_TOOL_NAME}.

## Parsing (in priority order)

1. **Leading token**: if the first whitespace-delimited token matches \`^\\d+[smhd]$\` (e.g. \`5m\`, \`2h\`), that's the interval; the rest is the prompt.
2. **Trailing "every" clause**: otherwise, if the input ends with \`every <N><unit>\` or \`every <N> <unit-word>\` (e.g. \`every 20m\`, \`every 5 minutes\`, \`every 2 hours\`), extract that as the interval and strip it from the prompt. Only match when what follows "every" is a time expression — \`check every PR\` has no interval.
3. **Default**: otherwise, interval is \`${DEFAULT_INTERVAL}\` and the entire input is the prompt.

If the resulting prompt is empty, show usage \`/loop [interval] <prompt>\` and stop — do not call ${CRON_CREATE_TOOL_NAME}.

Examples:
- \`5m /babysit-prs\` → interval \`5m\`, prompt \`/babysit-prs\` (rule 1)
- \`check the deploy every 20m\` → interval \`20m\`, prompt \`check the deploy\` (rule 2)
- \`run tests every 5 minutes\` → interval \`5m\`, prompt \`run tests\` (rule 2)
- \`check the deploy\` → interval \`${DEFAULT_INTERVAL}\`, prompt \`check the deploy\` (rule 3)
- \`check every PR\` → interval \`${DEFAULT_INTERVAL}\`, prompt \`check every PR\` (rule 3 — "every" not followed by time)
- \`5m\` → empty prompt → show usage

## Interval → native milliseconds

Supported suffixes: \`s\`, \`m\`, \`h\`, \`d\`. Convert the requested interval exactly to milliseconds (1000, 60000, 3600000, 86400000 multipliers). Do not round, convert to cron, or add jitter.

- \`5s\` → \`everyMs: 5000\`
- \`7m\` → \`everyMs: 420000\`
- \`90m\` → \`everyMs: 5400000\`
- \`2h\` → \`everyMs: 7200000\`

## Action

1. Call ${CRON_CREATE_TOOL_NAME} with:
   - \`everyMs\`: the exact interval in milliseconds
   - \`prompt\`: the parsed prompt from above, verbatim (slash commands are passed through unchanged)
   - \`recurring\`: \`true\`
   ${durableInstruction}
   - \`sessionTarget\`: \`continuation\`
   - \`wakeMode\`: \`next-heartbeat\`
2. Briefly confirm: the exact cadence, target, whether it is session-only or durable, the explicit expiry returned by the tool, and how to cancel with ${CRON_DELETE_TOOL_NAME}.
3. Do **not** execute the prompt immediately. The first run is \`anchor + everyMs\`. Only execute now if the user separately and explicitly asked for an immediate first run.

## Input

${args}`
}

export function registerLoopSkill(): void {
  registerBundledSkill({
    name: 'loop',
    description:
      'Run a prompt or slash command on a recurring interval (e.g. /loop 5m /foo, defaults to 10m)',
    whenToUse:
      'When the user wants to set up a recurring task, poll for status, or run something repeatedly on an interval (e.g. "check the deploy every 5 minutes", "keep running /babysit-prs"). Do NOT invoke for one-off tasks.',
    argumentHint: '[interval] <prompt>',
    userInvocable: true,
    isEnabled: isKairosCronEnabled,
    async getPromptForCommand(args) {
      const trimmed = args.trim()
      if (!trimmed) {
        return [{ type: 'text', text: LOOP_USAGE_MESSAGE }]
      }
      return [
        {
          type: 'text',
          text: buildLoopPrompt(trimmed, isDurableCronEnabled()),
        },
      ]
    },
  })
}
