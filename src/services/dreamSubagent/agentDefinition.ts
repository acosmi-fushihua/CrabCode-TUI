/**
 * W-MEMORY-DREAM-REBUILD v7 P6.1 (2026-05-25) — dream subagent identity.
 *
 * dream / extract / session-memory consolidation does not live on the
 * `AgentTool runAgent` path; it is triggered by the Rust orchestrator
 * (`acosmi-memory-orchestrator`) and reaches the TS side through one of
 * two routes:
 *
 *   1. The legacy `src/services/memoryRunners/dream/runner.ts` pipeline
 *      which calls `runForkedAgent(...)` directly (the v6 spike-6
 *      same-stopHook POC path);
 *   2. The reverse-IPC `memory/tier/llmCallRequest` notification path
 *      added in W-MEMORY-DREAM-REBUILD v7 P3.1, where the orchestrator
 *      broadcasts a Tier LLM call request and the TS subscriber spawns
 *      the SDK call (P3.2 onwards).
 *
 * Both routes need a stable agent identity so the task panel / telemetry
 * / agent-state surfaces can tell dream forks apart from generic
 * `general-purpose` or `Explore` subagents. This file is that identity.
 *
 * Deliberate exclusions:
 *
 * - **Not preloaded into `src/skills/bundled/index.ts`.** Tier policy
 *   (Tier-1 SessionMemory / Tier-2 ExtractMemories / Tier-3 AutoDream)
 *   lives in the Rust orchestrator and is not part of the TS skill
 *   registry. Re-introducing a `dream` bundled skill would resurrect the
 *   removed fork residue (see W-SKILL-SYSTEM-HEALTH-ROOTCAUSE PR-2,
 *   2026-05-22). The negative lock in
 *   `tests/unit/bundledSkillsInit.test.ts:99-102` keeps this invariant.
 *
 * - **Not registered with `BackgroundAgentScheduler`.** That scheduler owns
 *   the process-level concurrency budget for AgentTool fan-out. Dream forks are
 *   orchestrator-triggered and intentionally bypass the scheduler so
 *   they do not consume AgentTool slots.
 *
 * - **Not loaded through `loadAgentsDir`.** The on-disk agent dir is a
 *   user/project surface for `AgentTool`; dream is internal-only. The
 *   identity is exported as constants here and consumed directly by the
 *   runner via `overrides.agentType`.
 */

/** Stable wire identifier — consumed by the task panel, telemetry, and
 *  `agentTaskMetadata.ts` to label dream-runner fork results. Anything that
 *  needs to detect a dream subagent should `===` against this constant. */
export const DREAM_SUBAGENT_AGENT_TYPE = 'dream-subagent' as const

/** Human-readable description of the subagent's contract. Mirrored on the
 *  Rust side in `acosmi-memory-orchestrator` documentation; if the wire
 *  semantics ever change this constant is the telemetry-facing source. */
export const DREAM_SUBAGENT_AGENT_DESCRIPTION =
  'Rust orchestrator-triggered subagent for Tier-1/2/3 memory consolidation. ' +
  'LLM invocation only; data management in Rust.'

/** Type-narrowing helper: `true` iff the given `agentType` field is the
 *  dream-subagent identity. Mirrors `isBuiltInAgent` / `isCustomAgent`
 *  shape from `src/tools/AgentTool/loadAgentsDir.ts`. */
export function isDreamSubagentType(
  agentType: string | null | undefined,
): boolean {
  return agentType === DREAM_SUBAGENT_AGENT_TYPE
}
