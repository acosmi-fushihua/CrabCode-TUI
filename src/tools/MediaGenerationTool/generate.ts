/**
 * Media-generation orchestration.
 *
 * The async work that runs inside `MediaGenerationTool.call()`:
 *  - `generateImageMedia` (synchronous SDK call): select an image-generation
 *    model from the catalog, call `client.generateImage()`, obtain the bytes
 *    (decode `b64_json` or fetch `url`), save to disk, return a structured
 *    result.
 *  - `generateVideoMedia` (asynchronous SDK job): select a video-generation
 *    model, call `client.generateVideo()` to get a `taskId`, then poll
 *    `client.pollVideoTask()` (passing `durationSeconds` for per-duration
 *    billing) until `completed`/`failed`/timeout, download the video bytes and
 *    save to disk.
 *
 * Both honor the tool's `AbortSignal` for cancellation, and degrade cleanly
 * (returning an `error` outcome rather than throwing) when no capable model
 * exists or a download fails. Model selection is fully catalog-driven
 * and contains no hardcoded model ids.
 *
 * The result field names (`savedPath`, `revisedPrompt`, `videoUrl`, etc.) are
 * a stable runtime contract.
 */

import { Buffer } from 'node:buffer'
import type { MediaGenerationSelectionReason } from '../../utils/model/selectMediaGenerationModel.js'
import type { MediaGenerationDeps } from './deps.js'
import { DEFAULT_MEDIA_GENERATION_DEPS } from './deps.js'
import { MAX_TOOL_ARTIFACT_BYTES_BY_KIND } from '../../types/toolArtifact.js'
import {
  isImageDimensionWithinArtifactBudget,
  readImageArtifactSafetyMetadata,
  sniffSupportedImageMimeBytes,
} from '../../utils/imageArtifactSafety.js'
import {
  maybeResizeAndDownsampleImageBuffer,
  readImageHeaderDimensions,
} from '../../utils/imageResizer.js'

/** Total wall-clock cap for video polling before we report a timeout. */
export const VIDEO_POLL_MAX_WALL_MS = 5 * 60 * 1000
/** Delay between successive `pollVideoTask` calls. */
export const VIDEO_POLL_INTERVAL_MS = 5_000
export const MAX_GENERATED_IMAGE_BYTES = MAX_TOOL_ARTIFACT_BYTES_BY_KIND.image
export const MAX_GENERATED_VIDEO_BYTES = MAX_TOOL_ARTIFACT_BYTES_BY_KIND.video

export type ImageGenerationResult = {
  mediaType: 'image'
  savedPath: string
  revisedPrompt: string
  modelId: string
  requestId?: string
}

export type VideoGenerationResult = {
  mediaType: 'video'
  savedPath: string
  videoUrl: string
  taskId: string
  modelId: string
  durationSeconds?: number
}

export type MediaGenerationError = {
  mediaType: 'image' | 'video'
  error: string
}

export type MediaGenerationOutput =
  | ImageGenerationResult
  | VideoGenerationResult
  | MediaGenerationError

export function isMediaGenerationError(
  out: MediaGenerationOutput,
): out is MediaGenerationError {
  return 'error' in out
}

// ─── 媒体生成权益终态（文案即契约：media_entitlement_missing）──
// 网关对无生成权益的账号返 HTTP 402（流量包权益不足，按 slug 索引 bucket，见
// 同会话重试必然同败、非换参可解 → 符合终态枚举制收录判据。
// **返回契约的文档化例外**：本工具的 generate* 系列对一般错误"返回 error 输出
// 而不 throw"（返回式 tool_result 不带 is_error，§16 熔断器不可见），唯独权益
// 类错误必须 **throw** 抵达 toolExecution runtime catch，才能被分类计数熔断。
// terminalToolError.ts 按本前缀字符串判据分类；分类测试 import 本常量构造
// forward 用例。改文案必须同步判据和测试。
export const MEDIA_ENTITLEMENT_MISSING_MESSAGE_PREFIX =
  '媒体生成权益不足（HTTP 402）'

/** 判定网关权益类失败：SDK 错误对象的 status/statusCode，或 wire 文案指纹。 */
function isEntitlement402(err: unknown): boolean {
  if (!err || typeof err !== 'object') return false
  const e = err as { status?: unknown; statusCode?: unknown; message?: unknown }
  if (e.status === 402 || e.statusCode === 402) return true
  const msg = typeof e.message === 'string' ? e.message : ''
  return msg.includes('402') && (msg.includes('权益') || msg.includes('entitlement'))
}

function throwIfEntitlementMissing(err: unknown, kind: 'image' | 'video'): void {
  if (!isEntitlement402(err)) return
  const detail = err instanceof Error ? err.message : String(err)
  throw new Error(
    `${MEDIA_ENTITLEMENT_MISSING_MESSAGE_PREFIX}: ${kind} 生成被网关拒绝（${detail}）。` +
      '该账号当前无可用媒体生成权益，本会话内重试必然同样失败。' +
      '可引导用户前往 https://acosmi.com/zh/pricing 购买包含生成模型的套餐。',
  )
}

/** Map a selection diagnostic reason to a user-facing explanation. Only the
 *  three failure reasons reach here (a successful `'ok'` selection has a
 *  non-null modelId and never calls this); `'ok'` is handled defensively. */
function explainSelectionReason(
  reason: MediaGenerationSelectionReason,
  kind: 'image' | 'video',
): string {
  // W-MEDIA-GEN-IGNITION PR-2 — purchase guidance for the no-entitlement
  // paths: the tool prompt instructs the model to relay this plainly and NOT
  // retry (terminal for the session). URL is the user-authored acosmi.com
  // canonical account pricing page, not a guessed route.
  const purchaseGuidance =
    ' 可引导用户前往 https://acosmi.com/zh/pricing 购买包含生成模型的套餐。'
  switch (reason) {
    case 'no_models':
      return `No models are available in the account catalog.${purchaseGuidance}`
    case 'none_capable':
      return `No ${kind}-generation model is available in the account catalog.${purchaseGuidance}`
    case 'forced_not_capable':
      return `The requested model does not support ${kind} generation.`
    case 'ok':
      return `No ${kind}-generation model is available in the account catalog.${purchaseGuidance}`
  }
}

/**
 * Enforce the decode-budget gate WITHOUT rejecting a loosely-structured-but-
 * decodable image. The strict structural parser is tried first (cheap,
 * dependency-free, and yields a real frame count); when it cannot parse the
 * container, fall back to a header-only metadata read (no pixel decode → no
 * decode bomb). A managed generation model occasionally returns bytes a lenient
 * decoder accepts but our strict parser rejects (e.g. a JPEG without the exact
 * EOI trailer, a PNG with trailing bytes) — the old strict-only gate discarded
 * those AFTER the user was billed. Returns true only when real dimensions are
 * known AND within budget; false means genuinely over-budget (decode-bomb risk)
 * or undecodable.
 *
 * `readHeaderDimensions` is injectable ONLY so unit tests can exercise the
 * lenient (strict-parse-failed) branch: the native image processor is
 * unavailable in the single-process unit-test runtime (`maybeResizeAndDownsample`
 * silently rides its catch-fallback there), so a real header read returns null
 * and the loose branch could never be observed end-to-end. Production always
 * uses the default `readImageHeaderDimensions`. Exported for
 * tests/unit/media-generation-tool.test.ts.
 */
export async function isGeneratedImageWithinDecodeBudget(
  bytes: Buffer,
  mimeType: string,
  readHeaderDimensions: (
    b: Buffer,
  ) => Promise<{ width: number; height: number } | null> = readImageHeaderDimensions,
): Promise<boolean> {
  const strict = readImageArtifactSafetyMetadata(bytes, mimeType)
  if (strict) {
    return isImageDimensionWithinArtifactBudget(
      strict.width,
      strict.height,
      strict.frames,
    )
  }
  const dims = await readHeaderDimensions(bytes)
  if (!dims) return false
  // Header-only reads don't give a reliable animation frame count, but the
  // downsampler emits a single still frame (it never requests animated output),
  // so frames=1 matches the work budget of what actually gets written to disk.
  return isImageDimensionWithinArtifactBudget(dims.width, dims.height, 1)
}

async function prepareGeneratedImage(
  bytes: Uint8Array,
): Promise<{ bytes: Uint8Array; ext: string }> {
  const source = Buffer.from(bytes)
  const mimeType = sniffSupportedImageMimeBytes(source)
  if (mimeType === null) {
    // No recognizable image header at all — genuinely not a supported image.
    throw new Error('generation model returned an unsupported, malformed, or unsafe image')
  }
  if (!(await isGeneratedImageWithinDecodeBudget(source, mimeType))) {
    // Dimensions exceed the safe decode budget (real decode-bomb risk) or the
    // bytes are undecodable. Loosely-structured-but-decodable images are
    // admitted above and normalized by the downsampler below, so this rejects
    // only genuine bombs / garbage — not a valid paid generation.
    throw new Error('generation model returned an unsupported, malformed, or unsafe image')
  }
  const resized = await maybeResizeAndDownsampleImageBuffer(
    source,
    source.length,
    mimeType.slice('image/'.length),
  )
  const normalizedMime = `image/${resized.mediaType === 'jpg' ? 'jpeg' : resized.mediaType}`
  if (
    sniffSupportedImageMimeBytes(resized.buffer) !== normalizedMime ||
    !(await isGeneratedImageWithinDecodeBudget(resized.buffer, normalizedMime))
  ) {
    throw new Error('generated image failed host decode validation')
  }
  const ext = normalizedMime === 'image/jpeg'
    ? 'jpg'
    : normalizedMime.slice('image/'.length)
  return { bytes: resized.buffer, ext }
}

function isMp4Bytes(bytes: Uint8Array): boolean {
  return bytes.length >= 12 &&
    Buffer.from(bytes.subarray(4, 8)).toString('ascii') === 'ftyp'
}

async function fetchBytes(
  url: string,
  fetchImpl: MediaGenerationDeps['fetch'],
  maxBytes: number,
  signal?: AbortSignal,
): Promise<Uint8Array> {
  const res = await fetchImpl(url, { signal })
  if (!res.ok) {
    throw new Error(`download failed: HTTP ${res.status}`)
  }
  const contentLength = res.headers?.get('content-length')
  if (contentLength !== null && contentLength !== undefined) {
    const declaredBytes = Number(contentLength)
    if (Number.isFinite(declaredBytes) && declaredBytes > maxBytes) {
      throw new Error(`download exceeds the ${maxBytes} byte media limit`)
    }
  }
  if (res.body) {
    const reader = res.body.getReader()
    const chunks: Uint8Array[] = []
    let total = 0
    while (true) {
      const { done, value } = await reader.read()
      if (done) break
      if (!value || value.length === 0) continue
      total += value.length
      if (total > maxBytes) {
        await reader.cancel('media download exceeded size limit').catch(() => {})
        throw new Error(`download exceeds the ${maxBytes} byte media limit`)
      }
      chunks.push(value)
    }
    const bytes = new Uint8Array(total)
    let offset = 0
    for (const chunk of chunks) {
      bytes.set(chunk, offset)
      offset += chunk.length
    }
    return bytes
  }
  const buf = await res.arrayBuffer()
  if (buf.byteLength > maxBytes) {
    throw new Error(`download exceeds the ${maxBytes} byte media limit`)
  }
  return new Uint8Array(buf)
}

function decodeGeneratedImageBase64(value: string): Uint8Array {
  const maxChars = Math.ceil(MAX_GENERATED_IMAGE_BYTES / 3) * 4
  if (
    value.length > maxChars ||
    value.length % 4 === 1 ||
    !/^[A-Za-z0-9+/]*={0,2}$/.test(value)
  ) {
    throw new Error(`base64 image exceeds the ${MAX_GENERATED_IMAGE_BYTES} byte limit or is invalid`)
  }
  const bytes = Buffer.from(value, 'base64')
  if (bytes.length > MAX_GENERATED_IMAGE_BYTES) {
    throw new Error(`base64 image exceeds the ${MAX_GENERATED_IMAGE_BYTES} byte limit`)
  }
  return new Uint8Array(bytes)
}

export type GenerateImageArgs = {
  prompt: string
  width?: number
  height?: number
  style?: string
  forceModelId?: string | null
  signal?: AbortSignal
}

/**
 * Image generation (synchronous). Returns an `error` outcome instead of
 * throwing when no capable model exists or the download fails, so the tool
 * surfaces a clear message to the model rather than an uncaught exception.
 */
export async function generateImageMedia(
  args: GenerateImageArgs,
  deps: MediaGenerationDeps = DEFAULT_MEDIA_GENERATION_DEPS,
): Promise<MediaGenerationOutput> {
  const selection = deps.selectImageGenerationModel(
    deps.getImageGenerationModelCandidates(),
    { forceModelId: args.forceModelId ?? null },
  )
  if (!selection.modelId) {
    return {
      mediaType: 'image',
      error: explainSelectionReason(selection.reason, 'image'),
    }
  }

  try {
    const client = await deps.getAcosmiClient()
    const response = await client.generateImage(
      selection.modelId,
      {
        prompt: args.prompt,
        width: args.width,
        height: args.height,
        style: args.style,
      },
      args.signal,
    )

    let bytes: Uint8Array
    if (response.b64_json) {
      bytes = decodeGeneratedImageBase64(response.b64_json)
    } else if (response.url) {
      bytes = await fetchBytes(
        response.url,
        deps.fetch,
        MAX_GENERATED_IMAGE_BYTES,
        args.signal,
      )
    } else {
      return {
        mediaType: 'image',
        error: 'The generation model returned neither image bytes nor a URL.',
      }
    }

    const prepared = await prepareGeneratedImage(bytes)
    const savedPath = await deps.saveMediaToDisk(prepared)

    return {
      mediaType: 'image',
      savedPath,
      revisedPrompt: response.revised_prompt ?? args.prompt,
      modelId: selection.modelId,
      requestId: response.requestId,
    }
  } catch (err) {
    // 权益类失败是会话级确定性终态：throw 进熔断器（见 throwIfEntitlementMissing
    // 的返回契约例外说明）；其余错误维持"返回 error 输出"契约。
    throwIfEntitlementMissing(err, 'image')
    return {
      mediaType: 'image',
      error: `Image generation failed: ${
        err instanceof Error ? err.message : String(err)
      }`,
    }
  }
}

export type GenerateVideoArgs = {
  prompt: string
  resolution?: string
  duration?: number
  forceModelId?: string | null
  signal?: AbortSignal
}

/** Abortable sleep used between video poll attempts. */
function sleep(ms: number, signal?: AbortSignal): Promise<void> {
  return new Promise<void>((resolve, reject) => {
    if (signal?.aborted) {
      reject(new Error('aborted'))
      return
    }
    const timer = setTimeout(() => {
      signal?.removeEventListener('abort', onAbort)
      resolve()
    }, ms)
    const onAbort = () => {
      clearTimeout(timer)
      reject(new Error('aborted'))
    }
    signal?.addEventListener('abort', onAbort, { once: true })
  })
}

/**
 * Video generation (asynchronous). Submits the job, then polls
 * `pollVideoTask` (passing `durationSeconds` for per-duration billing) until
 * the task completes, fails, or the wall-clock cap is exceeded. Aborts the
 * loop when `args.signal` fires. Returns an `error` outcome instead of
 * throwing on failure/timeout/no-capable-model.
 */
export async function generateVideoMedia(
  args: GenerateVideoArgs,
  deps: MediaGenerationDeps = DEFAULT_MEDIA_GENERATION_DEPS,
  timing: { maxWallMs?: number; intervalMs?: number; now?: () => number } = {},
): Promise<MediaGenerationOutput> {
  const maxWallMs = timing.maxWallMs ?? VIDEO_POLL_MAX_WALL_MS
  const intervalMs = timing.intervalMs ?? VIDEO_POLL_INTERVAL_MS
  const now = timing.now ?? Date.now

  const selection = deps.selectVideoGenerationModel(
    deps.getVideoGenerationModelCandidates(),
    { forceModelId: args.forceModelId ?? null },
  )
  if (!selection.modelId) {
    return {
      mediaType: 'video',
      error: explainSelectionReason(selection.reason, 'video'),
    }
  }
  const modelId = selection.modelId

  try {
    const client = await deps.getAcosmiClient()
    const submitted = await client.generateVideo(
      modelId,
      {
        prompt: args.prompt,
        resolution: args.resolution,
        duration: args.duration,
      },
      args.signal,
    )

    const taskId = submitted.taskId
    // The submit call may already return a terminal state.
    let task = submitted
    const startedAt = now()

    while (task.status !== 'completed' && task.status !== 'failed') {
      if (args.signal?.aborted) {
        return { mediaType: 'video', error: 'Video generation was aborted.' }
      }
      if (now() - startedAt >= maxWallMs) {
        return {
          mediaType: 'video',
          error: `Video generation timed out after ${Math.round(
            maxWallMs / 1000,
          )}s (task ${taskId}).`,
        }
      }
      await sleep(intervalMs, args.signal)
      // MUST pass durationSeconds so the gateway can bill the real duration.
      task = await client.pollVideoTask(
        modelId,
        taskId,
        args.duration,
        args.signal,
      )
    }

    if (task.status === 'failed') {
      return {
        mediaType: 'video',
        error: `Video generation failed: ${task.error ?? 'unknown error'}`,
      }
    }

    const videoUrl = task.videoUrl
    if (!videoUrl) {
      return {
        mediaType: 'video',
        error: 'Video task completed but returned no video URL.',
      }
    }

    const bytes = await fetchBytes(
      videoUrl,
      deps.fetch,
      MAX_GENERATED_VIDEO_BYTES,
      args.signal,
    )
    if (!isMp4Bytes(bytes)) {
      return {
        mediaType: 'video',
        error: 'Video generation failed: downloaded content is not an MP4 file.',
      }
    }
    const savedPath = await deps.saveMediaToDisk({ bytes, ext: 'mp4' })

    return {
      mediaType: 'video',
      savedPath,
      videoUrl,
      taskId,
      modelId,
      durationSeconds: args.duration,
    }
  } catch (err) {
    if (args.signal?.aborted) {
      return { mediaType: 'video', error: 'Video generation was aborted.' }
    }
    // 权益类失败是会话级确定性终态：throw 进熔断器（同 image 路径）。
    throwIfEntitlementMissing(err, 'video')
    return {
      mediaType: 'video',
      error: `Video generation failed: ${
        err instanceof Error ? err.message : String(err)
      }`,
    }
  }
}
