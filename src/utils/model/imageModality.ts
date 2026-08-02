/**
 * Image-input modality classification.
 *
 * Shared by the queryModel media policy (services/api/mediaCapabilityPolicy.ts)
 * and document-parsing routing (FileReadTool xlsx / PDF branches). Lives in
 * utils/model so tools can depend on it without a tools → services/api edge.
 *
 * Three-state semantics mirror the SDK helper `modelSupportsImageInput`:
 *   - array including 'image'      → supported
 *   - non-empty array w/o 'image'  → text_only (explicitly declared text-only)
 *   - absent / empty array         → unknown   (not declared; do NOT assume)
 *
 * Catalog-driven: never infers modality from the model
 * name; reads only the gateway-provided `inputModalities` field.
 */

import { resolveCustomModelVisionDeclaration } from './customModelResolver.js'
import { isLocalModelReference } from './localModelReference.js'
import { getAccountBridgeRouteCapability } from '../../services/accountBridge/capabilityCache.js'
import { getMainLoopModel } from './model.js'
import {
  getExactModelCapability,
  type ModelCapability,
} from './modelCapabilities.js'

export type MediaModalityClass = 'supported' | 'text_only' | 'unknown'

/** Pure classifier over a capability record (zero I/O; unit-test surface). */
export function classifyImageModalityFromCapability(
  cap: Pick<ModelCapability, 'inputModalities'> | undefined,
): MediaModalityClass {
  const mods = cap?.inputModalities
  if (Array.isArray(mods) && mods.includes('image')) return 'supported'
  if (Array.isArray(mods) && mods.length > 0) return 'text_only'
  return 'unknown'
}

/**
 * Classify a model reference. Custom/local/Account Bridge namespaces are
 * handled FIRST (PR-7, 裁决 D9): they never borrow the gateway catalog, so the catalog
 * classifier would return `unknown`; the media policy treats that state as a
 * privacy boundary and never blind-sends images to a
 * local endpoint that 400s the whole turn. For those namespaces the user's
 * declared `supportsVision` decides — `true → supported`, absent/false →
 * `text_only` (fail-closed: degrading keeps the turn alive; a checkbox flip
 * restores full vision). Local models carry no capability declaration →
 * always `text_only`. Account Bridge uses the fresh exact-route capability;
 * a missing/null value stays `unknown` and never falls through to a same-text
 * gateway slug. Both gateway and non-gateway unknown states fail closed at the
 * media policy boundary; non-gateway references are additionally forbidden
 * from using a managed-gateway sidecar.
 */
export function classifyModelImageModality(model: string): MediaModalityClass {
  if (isLocalModelReference(model)) return 'text_only'
  const custom = resolveCustomModelVisionDeclaration(model)
  if (custom.isCustom) {
    return custom.supportsVision ? 'supported' : 'text_only'
  }
  const accountRoute = getAccountBridgeRouteCapability(model)
  if (accountRoute !== undefined) {
    if (accountRoute?.supportsVision === true) return 'supported'
    if (accountRoute?.supportsVision === false) return 'text_only'
    // A missing route or an explicitly unknown capability must never borrow
    // the managed gateway catalog. Keeping it unknown lets the selected
    // Account Bridge route remain the sole possible recipient. The media
    // policy strips the attachment when support cannot be proven.
    return 'unknown'
  }
  return classifyImageModalityFromCapability(getExactModelCapability(model))
}

/**
 * Modality of the CURRENT main-loop model. Document-parsing routing uses this
 * as its decision input; subagents running a different model approximate to
 * the main-loop model — the same deliberate approximation `isPDFSupported()`
 * already makes (per-request plumbing into tools is not worth the surface).
 */
export function mainLoopModelImageModality(): MediaModalityClass {
  return classifyModelImageModality(getMainLoopModel())
}
