import { registerBundledSkill } from '../bundledSkills.js'
import {
  BROWSER_AUTOMATION_SKILL_DESCRIPTION,
  BROWSER_AUTOMATION_SKILL_WHEN_TO_USE,
  getAutomationSurfaceRoutingGuidance,
  getBrowserFailurePreservationReminder,
} from '../../utils/browserAutomation/intentRoute.js'

const BROWSER_AUTOMATION_PROMPT = `# CrabCode browser automation

Default browser-automation entry point for the CrabCode agent. Backend: \`crabcode-browser\` — a native Rust daemon (Apache-2.0 fork of agent-browser). Drive an isolated Chromium session through the public Rust CLI.

${getAutomationSurfaceRoutingGuidance('browser')}

## Entry points

- User-facing CLI: \`crabcode browser ...\`
- Lower-level runtime path: \`acosmi browser ...\` (only when working inside the runtime)
- Prefer \`--json\` for read commands (status, profiles, tabs, snapshot, console, requests)

## Currently supported (Rust adapter → crabcode-browser daemon)

Session: \`status\`, \`start\`, \`stop\`, \`profiles\`, \`create-profile\`, \`delete-profile\`, \`reset-profile\`
Tabs: \`tabs\`, \`open\`, \`focus\`, \`close\`
Navigation: \`navigate\`, \`back\`, \`forward\`, \`reload\`
Inspection: \`snapshot\`, \`screenshot\`, \`pdf\`, \`console\`, \`errors\`, \`requests\` (with \`--filter <regex>\`, \`--clear\`, \`--static\`), \`network request <id>\`
Actions: \`click\`, \`type\`, \`press\`, \`hover\`, \`scrollintoview\`, \`drag\`, \`select\`, \`fill\`, \`upload\`, \`dialog\`, \`wait\`, \`evaluate\`, \`highlight\`
Storage/IO: \`cookies\`, \`storage\`, \`download\`, \`waitfordownload\`, \`trace\`, \`set\`

## Snapshot refs

Snapshot element refs are \`@eN\` (e.g. \`@e1\`). The adapter accepts either \`@eN\` or bare \`eN\` and normalises, but emitted refs read \`@eN\` — pass them back verbatim to \`click\` / \`hover\` / \`type\` etc.

## Network read pattern (id-based, single command)

1. \`crabcode browser requests --json [--filter <regex>] [--clear] [--static]\` — list captured requests and pick an \`id\`.
2. \`crabcode browser network request <id>\` — returns the combined request + response detail (headers + body) for that id in one call. There is no separate per-index \`request-headers\` / \`response-body\` command.

The legacy \`crabcode browser responsebody <pattern>\` form is deprecated and returns \`unsupported_action\` — use the id-based path above.

## Native capabilities

\`set\` (offline / headers / credentials / geolocation / media / device), \`cookies\`, \`storage\`, \`trace\`, and \`download\` are first-class native commands — no \`run-code\` JS shims required.

\`generate-locator\` is not exposed by this backend and returns \`unsupported_action\`.

## Custom binary

Set \`CRABCODE_BROWSER_BINARY=/abs/path/to/crabcode-browser\` to override the default lookup. Without it the adapter looks beside the running executable, in \`<exe>/bin/\`, and in \`~/.crabcode/bin/\`.

## Typical flow

1. Check the backend: \`crabcode browser status --json\` (reports \`"backend":"crabcode-browser"\`).
2. Start or reuse a profile: \`crabcode browser start --profile <name>\`.
3. Navigate: \`crabcode browser navigate <url> --profile <name>\`.
4. Inspect before acting: \`crabcode browser snapshot --json --profile <name>\`.
5. Act with stable selectors or \`@eN\` snapshot refs.
6. Capture evidence when useful: \`crabcode browser screenshot --full --profile <name>\`.
7. Stop when the task is complete: \`crabcode browser stop --profile <name>\`.

This pure-TUI surface is isolated. It does not attach to an embedded desktop
browser or to a browser extension, and it must never use a host-reserved
profile identity. Do not silently switch to another browser surface.

## Safety

- Do not present stealth, bot evasion, CAPTCHA bypass, anti-detection, rate-limit bypass, mass-follow, mass-like, scraping, spam, or synthetic engagement as product capabilities.
- High-risk actions (\`evaluate\`, \`upload\`, \`download\`, \`fill --submit\`, JS wait predicates) require explicit permission via \`CRABCODE_BROWSER_ALLOW_RISKY\` or the foreground permission flow.
- For authenticated social/media platforms, keep the user in control of account state, consent, and posting decisions.
- If a platform's terms are unclear, stop and ask for confirmation instead of inventing a workaround.

${getBrowserFailurePreservationReminder('crabcode-browser')}`

export function registerBrowserAutomationSkill(): void {
  registerBundledSkill({
    name: 'crabcode-browser',
    aliases: ['browser-automation'],
    description: BROWSER_AUTOMATION_SKILL_DESCRIPTION,
    whenToUse: BROWSER_AUTOMATION_SKILL_WHEN_TO_USE,
    allowedTools: ['Bash'],
    // Automation surfaces are selected before the first model round from a
    // verified local user turn. Model-authored Skill calls must not create a
    // second control surface or bypass origin-bound routing.
    disableModelInvocation: true,
    userInvocable: true,
    async getPromptForCommand(args) {
      let prompt = BROWSER_AUTOMATION_PROMPT
      if (args) {
        prompt += `\n\n## Task\n\n${args}`
      }
      return [{ type: 'text', text: prompt }]
    },
  })
}
