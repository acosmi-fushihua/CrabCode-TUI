import { feature } from './featurePolyfill.js'

/**
 * Single source of truth for "is the Workflow runtime reachable at all".
 *
 * Two independent flags can open it, and they open *different* amounts:
 *
 * - `WORKFLOW_SCRIPTS` (private preview) opens the full surface: discovery of
 *   plugin-authored workflow scripts plus the `/workflows` management command.
 * - `DEEP_RESEARCH` (GA) opens only the privileged bundled workflows that ship
 *   inside the binary. No plugin script becomes executable because of it.
 *
 * Every runtime call site that must exist for a *bundled* workflow to run —
 * tool registration, the agent-recursion guard, the background task type, the
 * permission-request renderer, the task dialog and the auto-mode classifier —
 * must gate on this predicate rather than on `WORKFLOW_SCRIPTS` directly.
 * Gating any one of them on the narrower flag is not a cosmetic difference:
 * missing the permission renderer breaks the approval flow mid-run, and
 * missing the task type makes the whole run invisible.
 *
 * `/workflows` is deliberately NOT in that set — it is an execution entry
 * point for arbitrary plugin workflows, so it stays on `WORKFLOW_SCRIPTS`.
 */
export function isWorkflowRuntimeEnabled(): boolean {
  return feature('WORKFLOW_SCRIPTS') || feature('DEEP_RESEARCH')
}
