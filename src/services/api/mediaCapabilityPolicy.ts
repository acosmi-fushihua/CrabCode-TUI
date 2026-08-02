/**
 * Capability-aware multimodal input policy (W-MULTIMODAL-INPUT).
 *
 * Root cause this fixes: CrabCode previously sent image blocks to whatever
 * model the turn used, with NO check of whether that model can consume images.
 * Only the desktop-automation subsystem consumed `inputModalities`; the main
 * conversation path (pasted images, @-mentions, Read-tool images, MCP image
 * tool-results, PDF page-images) blind-sent media. The upstream `@acosmi/
 * sdk-ts` forwards content to the gateway WITHOUT client-side modality
 * validation (its `buildRequestBody` is pass-through) and exposes NO stable
 * "modality rejected" error contract — so the client MUST gate modality
 * itself. The SDK's own helper `modelSupportsImageInput()` returns `false` for
 * a missing `inputModalities`, and its types doc states callers "must treat
 * missing as text-only/unknown, never assume image support".
 *
 * This module is the single capability-aware layer, applied once per turn at
 * the `queryModel` chokepoint (after message normalization, before withRetry),
 * where the ACTUAL per-request model is known (main loop AND subagents). It is
 * NOT on `sideQuery`'s path, so the vision sidecar's own image send is never
 * gated (no circular degradation).
 *
 * Three-state policy keyed on the per-request model's `inputModalities`
 * (catalog-driven, with no hardcoded model ids):
 *
 *   - `supported`  (modalities includes 'image')        → pass through, 0 change.
 *   - `text_only`  (non-empty modalities WITHOUT 'image')→ degrade each image to
 *                                                          a sidecar description
 *                                                          (if `degradeImage` is
 *                                                          provided) or an honest
 *                                                          text placeholder. The
 *                                                          image is removed.
 *   - `unknown`    (modalities absent / empty)            → fail closed: use an
 *                                                          exact same-provider
 *                                                          sidecar when proven,
 *                                                          otherwise replace the
 *                                                          image with an honest
 *                                                          placeholder.
 *
 * Document blocks (application/pdf) are NOT gated here: the gateway parses PDF
 * document blocks for all models, and gating them on image modality would
 * regress PDF on text-only / unknown models. PDF page-IMAGES (extracted JPEGs)
 * ARE image blocks and so flow through this same image policy.
 *
 * The transform is idempotent: re-running on already-degraded messages is a
 * no-op (placeholders are plain text blocks), so it is safe across retries and
 * mid-conversation model switches.
 */

import type {
  BetaContentBlockParam,
  BetaImageBlockParam,
  BetaToolResultBlockParam,
  TextBlockParam,
} from '../../types/api-types.js'
import type { AssistantMessage, UserMessage } from '../../types/message.js'
import {
  classifyModelImageModality,
  type MediaModalityClass,
} from '../../utils/model/imageModality.js'
import { getCachedModels } from '../../utils/model/modelCapabilities.js'
import { isNonGatewayModelReference } from '../../utils/model/nonGatewayModelReference.js'

// PR-2 (W-DOC-VISION-QUALITY-REMEDIATION): the classifier moved to
// utils/model/imageModality.ts so FileReadTool routing can share it without a
// tools → services/api edge. Re-exported here so existing callers are
// untouched.
export {
  classifyModelImageModality,
  type MediaModalityClass,
} from '../../utils/model/imageModality.js'

/**
 * Structural failure detail of one vision-sidecar attempt (W-VISION-CONSENT-
 * SURFACE PR-R2a, 2026-07-24 审计 F4). Kept structural (kind + optional
 * reason) rather than importing the sidecar's own `SidecarOutcome` union so
 * this lower policy layer never depends on the sidecar implementation; every
 * `SidecarOutcome` variant is assignable to it.
 */
export type ImageDegradeFailureInfo = { kind: string; reason?: string | null }

/**
 * Degrader result meaning "the sidecar ran and terminally failed for THIS
 * reason". The policy folds it into the honest placeholder (carrying the
 * reason) instead of a bare unexplained placeholder — before this, a missing
 * one-time consent and a genuinely absent vision model produced identical
 * text, and the model filled the vacuum with invented model names.
 */
export type ImageDegradeFailure = { failure: ImageDegradeFailureInfo }

/** Cap the candidate echo so a pathological catalog cannot bloat a message. */
const MAX_PLACEHOLDER_CANDIDATES = 8

/**
 * Vision-capable chat candidates from the LOCAL catalog snapshot only
 * (audit §7.3: no network). Runtime ids echoed here are runtime values, not
 * hardcoded brand literals (§硬约束 #1 — the :74-76 precedent).
 *
 * 2026-07-27: locked entries are INCLUDED (annotated), because this list is
 * advice, not a routing decision. `getCachedModels()` excludes them by
 * default, and on a production catalog where every vision model comes back
 * locked that left the placeholder saying "switch models" while naming none —
 * advice the user could not act on. Naming a model that needs an entitlement
 * is strictly more useful than naming nothing, provided the caveat travels
 * with it. The routing paths keep the default (locked-excluded) accessor.
 */
function catalogVisionCandidateIds(): string[] {
  try {
    return getCachedModels({ includeLocked: true })
      .filter(
        entry =>
          entry.chatRuntimeSupported !== false &&
          entry.inputModalities?.includes('image') === true,
      )
      .map(entry =>
        entry.locked === true ? `${entry.id} (needs entitlement)` : entry.id,
      )
  } catch {
    return []
  }
}

/**
 * Reason sentence appended to the honest placeholder when the sidecar
 * reported WHY it declined. Surface-neutral wording (PR-3c precedent): the
 * placeholder reaches TUI, non-interactive, and subagent surfaces.
 */
function visionFallbackReasonSentence(
  failure: ImageDegradeFailureInfo | undefined,
): string {
  if (!failure) return ''
  if (failure.kind === 'consent_required') {
    // 2026-07-27: no longer "same-provider" — an approved destination may now
    // be any vision-capable catalog model (chatMediaSidecar.ts). The wording
    // is part of the contract with the model reading it: it must not promise a
    // narrower fallback than the one that actually exists.
    return (
      ' A vision fallback is available but needs a one-time authorization: ' +
      'run /vision in the CLI, or enable and approve the visual sidecar under ' +
      'Settings → Computer Control in the app. Then resend the image.'
    )
  }
  // no_eligible_model is NOT rendered here: it routes to the dedicated
  // buildNoEligibleVisionPlaceholder (F3a) so the merged F3a/F4 system keeps
  // exactly ONE copy per cause (audit §九: 避免两套占位文案).
  return ` (vision fallback unavailable: ${failure.kind})`
}

/**
 * Honest placeholder that replaces an image when the current model is
 * explicitly text-only and no vision sidecar produced a description. The
 * runtime model id is interpolated (a runtime value, not a hardcoded brand
 * literal — §硬约束 #1 forbids hardcoding ids in source, not echoing the
 * resolved value). This is the SOLE production point of the placeholder —
 * do not add a second one (audit §7.3 PR-R2a).
 */
export function buildTextOnlyImagePlaceholder(
  model: string,
  failure?: ImageDegradeFailureInfo,
): TextBlockParam {
  // PR-3c: surface-neutral wording — "/model" is TUI-only vocabulary, but this
  // placeholder reaches TUI, non-interactive, and subagent surfaces.
  return {
    type: 'text',
    text:
      `[Image omitted: the current model "${model}" is text-only and cannot ` +
      `process image input. Switch to a vision-capable model in the model ` +
      `settings and resend the image.` +
      `${visionFallbackReasonSentence(failure)}]`,
  }
}

/** Unknown capability is a privacy boundary, not permission to blind-send. */
export function buildUnknownImagePlaceholder(
  model: string,
  failure?: ImageDegradeFailureInfo,
): TextBlockParam {
  return {
    type: 'text',
    text:
      `[Image omitted: image-input support for the current model "${model}" ` +
      `could not be verified. Select a model with an explicit image capability ` +
      `and resend the image.` +
      `${visionFallbackReasonSentence(failure)}]`,
  }
}

/**
 * 2026-07-24 F3a（M3 缺口）：placeholder that CARRIES the reason when the
 * vision sidecar declined with `no_eligible_model`. Same-provider is the
 * AUTOMATIC selector's boundary (`chatMediaSidecar.ts`), so a text-only main
 * model whose provider has no vision-capable sibling degrades honestly — but
 * before this, the reason only reached a console.warn, and neither the model
 * nor the user learned that switching the MAIN model would make images
 * readable. Mac 实证（2026-07-23 turn1）：模型把占位文字如实转述给用户，这条
 * 转述链有效 —— 可行动的引导放进占位文字即可闭环，不加旁路通知。
 *
 * 2026-07-27: the copy names BOTH exits, because there are now two. Switching
 * the main model was the only one while the sidecar refused to cross provider
 * boundaries; approving an exact destination now also works, and for the
 * default model — whose provider ships no vision sibling at all — it is the
 * only one that keeps the current conversation on its model. Naming just one
 * exit is what made this placeholder read as a dead end.
 *
 * §硬约束 #1: only the runtime model id is interpolated — never a hardcoded
 * model-name literal suggesting a specific replacement.
 */
export function buildNoEligibleVisionPlaceholder(
  model: string,
  // PR-R2a (F4 合流): echo the catalog-wide image∩chatCapable candidate ids so
  // "switch the main model" is actionable — runtime values from the local
  // snapshot only, still zero brand literals in SOURCE (§硬约束 #1; the brand
  // tripwire test pins the static copy with candidates=[]).
  candidates: string[] = catalogVisionCandidateIds(),
): TextBlockParam {
  const shown = candidates.slice(0, MAX_PLACEHOLDER_CANDIDATES).join(', ')
  const overflow =
    candidates.length > MAX_PLACEHOLDER_CANDIDATES
      ? ` (+${candidates.length - MAX_PLACEHOLDER_CANDIDATES} more)`
      : ''
  const candidatesSentence =
    candidates.length > 0
      ? ` Vision-capable models currently in the catalog: ${shown}${overflow}.`
      : ''
  return {
    type: 'text',
    text:
      `[Image omitted: the current model "${model}" is text-only, and no ` +
      `vision-capable model from the same provider is available on this ` +
      `account to describe the image. Two ways to read it: switch the main ` +
      `model to one with image support in the model settings, or authorize a ` +
      `vision fallback destination (run /vision in the CLI, or Settings → ` +
      `Computer Control in the app) — an approved destination may be from a ` +
      `different provider. Then resend the image.` +
      `${candidatesSentence}]`,
  }
}

export type MediaDescriptionSource =
  | 'fresh'
  | 'memory_cache'
  | 'disk_cache'

export type ImageDegradeResult = {
  block: TextBlockParam
  source: MediaDescriptionSource
}

type CompatibleImageDegradeResult = TextBlockParam | ImageDegradeResult

/**
 * Optional async per-image degrader (the chat vision sidecar). Returns a
 * replacement text block (e.g. a textual description of the image produced by
 * a vision model via `sideQuery`), a `{ failure }` carrying WHY the sidecar
 * terminally declined (PR-R2a — folded into the honest placeholder), or
 * `null` for legacy callers to fall back to the reasonless placeholder.
 */
export type ImageDegrader = (
  image: BetaImageBlockParam,
  ctx: { model: string },
) => Promise<CompatibleImageDegradeResult | ImageDegradeFailure | null>

/**
 * Bounded parallelism for sidecar degradation (PR-3b, 裁决 D5). 3 — the
 * sidecar rides `sideQuery` (NOT AgentTool), so it does not touch the
 * BackgroundAgentScheduler budget (§8's semaphore binds AgentTool only, per
 * the contract's own text), and sideQuery already has bounded transient-429
 * retry (X1). 3 concurrent one-shot calls is not a fan-out; a 20-page scan
 * drops from ~20×T sequential to ~7×T wall-clock.
 */
export const MEDIA_DEGRADE_CONCURRENCY = 3

export type MediaCapabilityPolicyDeps = {
  /** Injectable for tests; defaults to the catalog-driven classifier. */
  classify?: typeof classifyModelImageModality
  /** Vision sidecar; when absent, text-only images degrade to placeholders. */
  degradeImage?: ImageDegrader
  /**
   * Synchronous description-cache probe (PR-3b): resolves already-described
   * images without occupying a concurrency slot. Wired to
   * `peekChatMediaSidecarCache` by queryModel; absent in placeholder-only use.
   */
  peekCached?: (
    image: BetaImageBlockParam,
    ctx: { model: string },
  ) => CompatibleImageDegradeResult | null
  /**
   * 2026-07-24 F3a: optional override for the honest-placeholder text used
   * when the sidecar produced no description. Returning `null` falls back to
   * the default builders. Counting semantics are unchanged — the replacement
   * still counts as `placeholderFallback` (`sidecarDescribed +
   * placeholderFallback === degradedImages` stays true), so telemetry cannot
   * mistake an explained placeholder for a working sidecar.
   */
  buildDegradePlaceholder?: (
    image: BetaImageBlockParam,
    ctx: { model: string; modality: MediaModalityClass },
  ) => TextBlockParam | null
}

/** Minimal worker-pool map (no third-party dep), order-preserving. */
async function mapWithConcurrency<T>(
  items: T[],
  limit: number,
  fn: (item: T, index: number) => Promise<void>,
): Promise<void> {
  let next = 0
  const workers = Array.from(
    { length: Math.max(1, Math.min(limit, items.length)) },
    async () => {
      while (true) {
        const i = next++
        if (i >= items.length) return
        await fn(items[i]!, i)
      }
    },
  )
  await Promise.all(workers)
}

export type MediaCapabilityPolicyResult = {
  messages: (UserMessage | AssistantMessage)[]
  /** Classification of the per-request model. */
  modality: MediaModalityClass
  /** Total image blocks seen (top-level + tool_result-nested). */
  imageCount: number
  /** How many image blocks were degraded/stripped (text_only or unknown). */
  degradedImages: number
  /**
   * Of the degraded images, how many got a REAL vision-sidecar description
   * (the fallback worked) vs. fell back to the honest text placeholder (the
   * sidecar was off / found no eligible model / failed). Splitting these is
   * the V2 observability fix: before, a broken vision fallback and a working
   * one both merely incremented `degradedImages`, so the caller's telemetry
   * could not tell them apart. `sidecarDescribed + placeholderFallback ===
   * degradedImages`.
   */
  sidecarDescribed: number
  placeholderFallback: number
  /** Fresh sideQuery calls. Only this count can imply an extra model charge. */
  freshDescriptions: number
  /** Reuse from the current process's private LRU. */
  memoryCacheHits: number
  /** Reuse from the private TTL-bounded on-disk cache. */
  diskCacheHits: number
  /** Exact plural name carried across the public protocol. */
  placeholderFallbacks: number
  /**
   * PR-R2a: per-outcome-kind tally of the placeholder fallbacks (e.g.
   * `{ consent_required: 2 }`). Legacy `null` degrader results contribute
   * nothing — the record only aggregates carried failure reasons, so the TUI
   * can render "why" without guessing. Internal envelope + TUI only; the
   * worker's protocol stamp deliberately does NOT pick this field up.
   */
  placeholderFallbackKinds: Record<string, number>
}

function normalizeDegradeResult(
  result: CompatibleImageDegradeResult,
  legacySource: MediaDescriptionSource,
): ImageDegradeResult {
  if ('block' in result && 'source' in result) return result
  return { block: result, source: legacySource }
}

function isImage(block: BetaContentBlockParam): block is BetaImageBlockParam {
  return block.type === 'image'
}

function isToolResult(
  block: BetaContentBlockParam,
): block is BetaToolResultBlockParam {
  return block.type === 'tool_result'
}

/**
 * Apply the capability-aware media policy to a normalized message list for a
 * specific per-request model. See module docs for the three-state policy.
 *
 * Pure with respect to `messages` (returns a new array only when something
 * changed; otherwise returns the input reference). The only impurity is the
 * optional `degradeImage` sidecar call.
 */
export async function applyMediaCapabilityPolicy(
  messages: (UserMessage | AssistantMessage)[],
  model: string,
  deps: MediaCapabilityPolicyDeps = {},
): Promise<MediaCapabilityPolicyResult> {
  const classify = deps.classify ?? classifyModelImageModality
  const modality = classify(model)

  // Count images regardless of action (telemetry + early-out).
  let imageCount = 0
  for (const msg of messages) {
    const content = msg.message.content
    if (!Array.isArray(content)) continue
    for (const block of content) {
      if (isImage(block)) imageCount++
      else if (isToolResult(block) && Array.isArray(block.content)) {
        for (const nested of block.content as BetaContentBlockParam[]) {
          if (isImage(nested)) imageCount++
        }
      }
    }
  }

  // Only an explicit image capability may receive pixels. Unknown/custom/
  // dangling references fail closed into a sidecar (when an exact same-provider
  // route can be proven) or an honest placeholder.
  if (modality === 'supported' || imageCount === 0) {
    return {
      messages,
      modality,
      imageCount,
      degradedImages: 0,
      sidecarDescribed: 0,
      placeholderFallback: 0,
      freshDescriptions: 0,
      memoryCacheHits: 0,
      diskCacheHits: 0,
      placeholderFallbacks: 0,
      placeholderFallbackKinds: {},
    }
  }

  // text_only/unknown: degrade every image (top-level + tool_result-nested).
  //
  // PR-3b (F1): two-pass, bounded-parallel. Pass 1 collects every image
  // occurrence in traversal order; cache hits (peekCached) resolve
  // synchronously, misses run through the sidecar with
  // MEDIA_DEGRADE_CONCURRENCY workers; pass 2 splices the replacements back
  // in the same traversal order. Previously each image awaited the full
  // sidecar round-trip sequentially — a 20-page scan meant ~20×T wall-clock.
  const occurrences: BetaImageBlockParam[] = []
  for (const msg of messages) {
    const content = msg.message.content
    if (!Array.isArray(content)) continue
    for (const block of content) {
      if (isImage(block)) occurrences.push(block)
      else if (isToolResult(block) && Array.isArray(block.content)) {
        for (const nested of block.content as BetaContentBlockParam[]) {
          if (isImage(nested)) occurrences.push(nested)
        }
      }
    }
  }

  let sidecarDescribed = 0
  let placeholderFallback = 0
  let freshDescriptions = 0
  let memoryCacheHits = 0
  let diskCacheHits = 0
  const placeholderFallbackKinds: Record<string, number> = {}
  const replacements: TextBlockParam[] = new Array(occurrences.length)
  const missIndexes: number[] = []
  // Account Bridge attachments are bound to one explicitly selected account
  // route. A managed-gateway vision sidecar (including a cached description)
  // would be a second recipient and violate that routing/privacy contract.
  // Known text-only account routes therefore get only the honest local
  // placeholder; supported/unknown routes already returned above unchanged.
  const allowVisionSidecar = !isNonGatewayModelReference(model)
  for (let i = 0; i < occurrences.length; i++) {
    const hit = allowVisionSidecar
      ? (deps.peekCached?.(occurrences[i]!, { model }) ?? null)
      : null
    if (hit) {
      const normalized = normalizeDegradeResult(hit, 'memory_cache')
      replacements[i] = normalized.block
      sidecarDescribed++
      if (normalized.source === 'memory_cache') memoryCacheHits++
      else if (normalized.source === 'disk_cache') diskCacheHits++
      else freshDescriptions++
    } else {
      missIndexes.push(i)
    }
  }
  await mapWithConcurrency(missIndexes, MEDIA_DEGRADE_CONCURRENCY, async i => {
    const viaSidecar = allowVisionSidecar && deps.degradeImage
      ? await deps.degradeImage(occurrences[i]!, { model })
      : null
    // Counter increments are safe: single-threaded event loop, no shared
    // intermediate state between the two lines.
    if (viaSidecar && 'failure' in viaSidecar) {
      // PR-R2a: the sidecar terminally declined and said why — fold the
      // reason into the placeholder. `no_eligible_model` routes to the F3a
      // dedicated builder (one copy per cause); other kinds append their
      // reason sentence to the modality placeholder. This per-image channel
      // takes precedence over the turn-level buildDegradePlaceholder hook
      // below, which remains the tested surface for legacy null degraders.
      const failure = viaSidecar.failure
      placeholderFallback++
      placeholderFallbackKinds[failure.kind] =
        (placeholderFallbackKinds[failure.kind] ?? 0) + 1
      replacements[i] =
        failure.kind === 'no_eligible_model'
          ? buildNoEligibleVisionPlaceholder(model)
          : modality === 'text_only'
            ? buildTextOnlyImagePlaceholder(model, failure)
            : buildUnknownImagePlaceholder(model, failure)
    } else if (viaSidecar) {
      const normalized = normalizeDegradeResult(viaSidecar, 'fresh')
      sidecarDescribed++
      replacements[i] = normalized.block
      if (normalized.source === 'memory_cache') memoryCacheHits++
      else if (normalized.source === 'disk_cache') diskCacheHits++
      else freshDescriptions++
    } else {
      placeholderFallback++
      replacements[i] =
        deps.buildDegradePlaceholder?.(occurrences[i]!, { model, modality }) ??
        (modality === 'text_only'
          ? buildTextOnlyImagePlaceholder(model)
          : buildUnknownImagePlaceholder(model))
    }
  })
  const degraded = occurrences.length

  let cursor = 0
  const nextReplacement = (): TextBlockParam => replacements[cursor++]!

  const newMessages: (UserMessage | AssistantMessage)[] = []
  for (const msg of messages) {
    const content = msg.message.content
    if (!Array.isArray(content)) {
      newMessages.push(msg)
      continue
    }
    let changed = false
    const newContent: BetaContentBlockParam[] = []
    for (const block of content) {
      if (isImage(block)) {
        newContent.push(nextReplacement())
        changed = true
      } else if (isToolResult(block) && Array.isArray(block.content)) {
        let nestedChanged = false
        const nestedContent: BetaContentBlockParam[] = []
        for (const nested of block.content as BetaContentBlockParam[]) {
          if (isImage(nested)) {
            nestedContent.push(nextReplacement())
            nestedChanged = true
          } else {
            nestedContent.push(nested)
          }
        }
        if (nestedChanged) {
          newContent.push({
            ...block,
            content: nestedContent as BetaToolResultBlockParam['content'],
          })
          changed = true
        } else {
          newContent.push(block)
        }
      } else {
        newContent.push(block)
      }
    }
    if (changed) {
      // Only the content array was swapped (role/discriminant preserved); the
      // spread widens the union past TS's narrowing, so cast like the sibling
      // `stripExcessMediaItems` does. Mirrors that file's idiom.
      newMessages.push({
        ...msg,
        message: { ...msg.message, content: newContent },
      } as unknown as UserMessage | AssistantMessage)
    } else {
      newMessages.push(msg)
    }
  }

  return {
    messages: newMessages,
    modality,
    imageCount,
    degradedImages: degraded,
    sidecarDescribed,
    placeholderFallback,
    freshDescriptions,
    memoryCacheHits,
    diskCacheHits,
    placeholderFallbacks: placeholderFallback,
    placeholderFallbackKinds,
  }
}
