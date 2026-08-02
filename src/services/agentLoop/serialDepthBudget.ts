/**
 * Serial-depth budget for a single agent
 * `queryLoop` (src/query.ts).
 *
 * This complements the concurrent fan-out budget
 * (`BackgroundAgentScheduler`): that scheduler bounds how many agents run at once; this
 * bounds how far ONE agent recurses in a single turn chain. The 2026-07-01 xlsx
 * incident (221 turns / 55 min / 1.6 亿 tokens, zero output) was serial-depth
 * failure — the concurrent budget was irrelevant.
 */

/**
 * No-progress watchdog: K consecutive turns where EVERY tool_result is
 * `is_error` (zero successful output) → hard stop. The incident's 221 turns
 * were entirely error results, so K=5 is a high-precision, near-zero-false-
 * positive signature (any legitimate long session lands successful tool results).
 */
export const NO_PROGRESS_TURN_LIMIT = 5

/**
 * Coarse safety ceiling for open-ended interactive TUI sessions that
 * otherwise carry no turn bound at all. High by design: the fine-grained gates
 * (terminal circuit break N+1=3, no-progress watchdog K=5) catch real failures
 * within the first handful of turns, so this only bounds the "mixed-progress
 * runaway" pathology — legitimate heavy sessions (large refactors, multi-file
 * migrations) can reach into the hundreds of turns and must NOT be culled.
 * env `CRABCODE_MAX_TURNS_INTERACTIVE` overrides (0 disables). headless
 * `--max-turns` and subagent (200) are unaffected.
 */
export const DEFAULT_INTERACTIVE_MAX_TURNS = 300

/**
 * Resolve the interactive turn ceiling. Returns `undefined` when disabled (env
 * set to 0) so callers can leave `maxTurns` unset.
 *
 * Injected at the two REAL interactive entry points — the REPL `query()` call
 * and headless-runtime's QueryEngine construction when `metadata.interactive`
 * (audit deviation B, 2026-07-01): QueryEngine's constructor is NOT a single
 * interactive chokepoint (its `querySource` is always `'sdk'`, shared by
 * butler and headless `--print`; and the TUI REPL bypasses QueryEngine
 * entirely). A malformed env value falls back to the default (fail-safe —
 * never silently fail open to unbounded).
 */
/**
 * True for the TUI interactive main loop, identified by its `querySource`
 * (`repl_main_thread` and its output-style variants). This is a reliable,
 * already-in-`QueryParams` signal — it covers the TUI without threading a flag
 * through the deeply-indirected REPL → processUserInput turn assembly. The
 * butler carries `querySource: 'sdk'` (shared with headless `--print`), so it
 * is flagged separately via `QueryParams.isInteractive` from headless-runtime.
 */
export function isInteractiveMainLoopSource(querySource: string): boolean {
  return querySource.startsWith('repl_main_thread')
}

export function resolveInteractiveMaxTurns(): number | undefined {
  const raw = process.env.CRABCODE_MAX_TURNS_INTERACTIVE
  if (raw !== undefined && raw !== '') {
    const parsed = Number(raw)
    if (Number.isFinite(parsed) && parsed >= 0) {
      return parsed === 0 ? undefined : Math.floor(parsed)
    }
    // malformed → fall through to the default (fail-safe, not fail-open).
  }
  return DEFAULT_INTERACTIVE_MAX_TURNS
}

/**
 * W-CRON-SESSION-INJECTION-ISOLATION L5 — within-loop auto-injection runaway
 * guard: the §16 family's injection-side companion to the no-progress
 * watchdog. If a SINGLE continuous `queryLoop` invocation drains the SAME
 * auto-injected (isMeta) command content this many turns in a row — with no
 * intervening turn that drained different / no auto-injection — the loop is
 * re-consuming a self-replenishing system injection (a cron / channel / proxy
 * feedback loop) and is hard-stopped fail-loud.
 *
 * Deliberately LOOP-LOCAL and reset-on-idle: a legitimately RECURRING cron gets
 * a fresh loop invocation per scheduled fire (the session returns to idle
 * between fires), so its counter never accumulates and it can NEVER be culled
 * by this gate. This catches only a runaway WITHIN one uninterrupted agentic
 * turn chain. Cross-turn rate-limiting of legitimate recurrence is a separate
 * product-policy decision, intentionally out of scope. The threshold is high so
 * ordinary turns (which don't re-drain identical isMeta content) never trip it.
 * The incident this project fixes is stopped upstream by session-scoped queue
 * confinement (L2/L3) + deleting the leaked jobs (L0); this is defense in depth.
 */
export const AUTO_INJECT_REPEAT_LIMIT = 8

/**
 * Stable content key for the auto-injected (isMeta) STRING commands in a drain
 * snapshot, or `null` when the drain carried no such command. Only isMeta
 * string values are keyed: those are the system/auto-injected prompts (cron,
 * proactive, channel). Human keyboard input is never isMeta, so it can never
 * feed the repeat counter; `ContentBlockParam[]` (image) values are ignored.
 */
export function autoInjectContentKey(
  cmds: ReadonlyArray<{ value: unknown; isMeta?: boolean }>,
): string | null {
  const texts: string[] = []
  for (const cmd of cmds) {
    if (cmd.isMeta === true && typeof cmd.value === 'string') texts.push(cmd.value)
  }
  return texts.length > 0 ? texts.join('\n') : null
}
