/**
 * Is the gateway's per-model `locked` flag a client-side veto?
 *
 * `model/list` returns a `locked` boolean per managed model. The client used to
 * treat it as authoritative everywhere: the model picker disabled the row, the
 * `queryModel` pre-flight refused to send, and the vision sidecar dropped the
 * model from its candidate set. That was correct only while the flag actually
 * predicted a server-side refusal.
 *
 * Production verification established that it does not:
 *
 *   - The gateway decides catalog availability by "does the account hold an
 *     entitlement bucket for this model", while admission and billing run on
 *     the credit chain, which does NOT require a per-model bucket.
 *   - The grant side has never created buckets for the paid-tier models, and
 *     paid plans carry no `allowedModels` in `dist_limit_policy` — so a paying
 *     member is handed exactly the free user's single bucket and everything
 *     else comes back `locked: true`.
 *   - Usage events prove those "locked" models are callable: accounts holding
 *     only a free-tier bucket completed hundreds of requests against them.
 *
 * So for an account at or above the membership floor the flag carries no
 * information, and enforcing it locally turns a display defect into a hard
 * functional block. For accounts BELOW the floor it still predicts correctly —
 * the free plan's `allowedModels` whitelist really is enforced at hold time —
 * so the flag stays authoritative there.
 *
 * This module is the single place that decision lives. Four surfaces consume
 * it (TUI picker, `queryModel` pre-flight, chat vision sidecar, vision
 * candidate echo); duplicating the reasoning at each site is exactly the drift
 * pattern that produced the stale `model_id` references in §4 of the same
 * audit.
 *
 * Deliberately dependency-free beyond the sibling resolver: this is bundled
 * into the Tauri webview as well as the TUI/worker processes.
 */
import {
  resolveCustomModelEntitlement,
  type CustomModelEntitlementInput,
} from './customModels.js'

/**
 * True when the client should keep treating `locked === true` as a hard local
 * veto for this account.
 *
 * Reuses {@link resolveCustomModelEntitlement} rather than re-deriving a
 * membership predicate: the question ("is this an active member at or above
 * the Plus floor?") is identical, and the custom / local / Account Bridge rows
 * already share that resolver. Notably it excludes `GO` (rank below the floor)
 * — the one paid plan that really does carry an `allowedModels` whitelist, and
 * therefore the one paid plan whose locks are genuine.
 *
 * Unknown membership, unknown plan code and free accounts all fail closed to
 * `true` (enforce), inheriting the resolver's fail-closed posture.
 */
export function shouldEnforceGatewayLockedFlag(
  input: CustomModelEntitlementInput = {},
): boolean {
  return !resolveCustomModelEntitlement(input).allowed
}
