/**
 * MediaGenerationTool.
 *
 * A single agent-driven tool that generates either an image (synchronous) or a
 * video (asynchronous) from a text prompt, via the catalog-selected managed
 * generation model. One tool with a `mediaType` discriminator (rather than two
 * tools) keeps the model's tool surface minimal: image and video share the
 * same prompt + model-selection + save-to-disk flow and differ only in which
 * SDK method is called and which optional fields apply.
 *
 * Opt-in: registered only when `isMediaGenerationEnabled()`
 * (`CRABCODE_MEDIA_GENERATION=1`), because generation is billed per call.
 *
 * Model selection is fully catalog-driven, with no
 * hardcoded model ids. The structured result field names (`savedPath`,
 * `revisedPrompt`, `videoUrl`, ...) are the stable runtime contract.
 */

import { z } from 'zod/v4'
import { basename, extname } from 'node:path'
import type { PermissionResult } from '../../utils/permissions/PermissionResult.js'
import { buildTool, type ToolDef } from '../../Tool.js'
import { lazySchema } from '../../utils/lazySchema.js'
import { DEFAULT_MEDIA_GENERATION_DEPS, type MediaGenerationDeps } from './deps.js'
import {
  generateImageMedia,
  generateVideoMedia,
  isMediaGenerationError,
  type MediaGenerationOutput,
} from './generate.js'
import {
  getMediaGenerationPrompt,
  MEDIA_GENERATION_TOOL_NAME,
} from './prompt.js'
import { createToolPresentationDelegates } from '../toolPresentationRegistry.js'

const inputSchema = lazySchema(() =>
  z.strictObject({
    mediaType: z
      .enum(['image', 'video'])
      .describe(
        "What to generate: 'image' (synchronous still image) or 'video' (asynchronous, polled until done).",
      ),
    prompt: z
      .string()
      .min(1)
      .describe('A detailed text description of the media to generate.'),
    // Image-only options (ignored for video).
    width: z
      .number()
      .int()
      .positive()
      .optional()
      .describe('Image only: output width in pixels (default model-defined).'),
    height: z
      .number()
      .int()
      .positive()
      .optional()
      .describe('Image only: output height in pixels (default model-defined).'),
    style: z
      .string()
      .optional()
      .describe('Image only: optional style hint (e.g. "photographic").'),
    // Video-only options (ignored for image).
    resolution: z
      .string()
      .optional()
      .describe('Video only: optional resolution hint (e.g. "1080p").'),
    duration: z
      .number()
      .positive()
      .optional()
      .describe('Video only: desired length in seconds (also used for billing).'),
  }),
)
type InputSchema = ReturnType<typeof inputSchema>
type Input = z.infer<InputSchema>

export type Output = MediaGenerationOutput

export function buildMediaGenerationTool(
  deps?: MediaGenerationDeps,
) {
  return buildTool({
    name: MEDIA_GENERATION_TOOL_NAME,
    searchHint: 'generate create image video from prompt',
    maxResultSizeChars: 100_000,
    async description(input) {
      const kind = input.mediaType === 'video' ? 'video' : 'image'
      return `CrabCode wants to generate ${
        kind === 'video' ? 'a video' : 'an image'
      }: ${input.prompt}`
    },
    userFacingName() {
      return 'Generate Media'
    },
    getActivityDescription(input) {
      return input?.mediaType === 'video'
        ? 'Generating video'
        : 'Generating image'
    },
    isEnabled() {
      return true
    },
    isConcurrencySafe() {
      return false
    },
    isReadOnly() {
      // Writes a file to disk under generated-media/.
      return false
    },
    toAutoClassifierInput(input) {
      return `${input.mediaType}: ${input.prompt}`
    },
    get inputSchema(): InputSchema {
      return inputSchema()
    },
    async checkPermissions(_input): Promise<PermissionResult> {
      return {
        behavior: 'passthrough',
        message: 'GenerateMedia requires permission.',
        suggestions: [
          {
            type: 'addRules',
            rules: [{ toolName: MEDIA_GENERATION_TOOL_NAME }],
            behavior: 'allow',
            destination: 'localSettings',
          },
        ],
      }
    },
    async prompt() {
      return getMediaGenerationPrompt()
    },
    ...createToolPresentationDelegates(MEDIA_GENERATION_TOOL_NAME, [
      'renderToolUseMessage',
      'renderToolResultMessage',
    ]),
    async validateInput(_input, context) {
      // 一轮一调的代码对偶（2026-07-04 审计 PR-8）：工具 prompt 承诺"每轮至多
      // 一次生成、失败不重试"是计费行为约束——prompt 级承诺必须有代码闸（§16
      // 精神）。本轮已有一次**成功**生成 → 确定性校验拒绝（不是终态签名；失败
      // 后的重试由 prompt 约束 + 权益终态签名分层处理）。
      const successesThisTurn = countTurnGenerateMediaSuccesses(context.messages)
      if (successesThisTurn >= 1) {
        return {
          result: false,
          message:
            '本轮已完成一次媒体生成。GenerateMedia 每轮至多调用一次（计费约束）：请在下一轮继续生成，或把多个需求合并进一次生成的提示词。',
          errorCode: 1,
        }
      }
      return { result: true }
    },
    async call(input: Input, context) {
      const signal = context.abortController.signal
      // Resolve defaults at execution time. Several catalog/runtime modules
      // reference the registered tool surface during startup; evaluating the
      // default dependency object while that graph is still initializing
      // creates a temporal-dead-zone cycle.
      const runtimeDeps = deps ?? DEFAULT_MEDIA_GENERATION_DEPS
      const data: Output =
        input.mediaType === 'video'
          ? await generateVideoMedia(
              {
                prompt: input.prompt,
                resolution: input.resolution,
                duration: input.duration,
                signal,
              },
              runtimeDeps,
            )
          : await generateImageMedia(
              {
                prompt: input.prompt,
                width: input.width,
                height: input.height,
                style: input.style,
                signal,
              },
              runtimeDeps,
            )
      if (isMediaGenerationError(data)) return { data }
      const extension = extname(data.savedPath).toLowerCase()
      const mimeType = data.mediaType === 'video'
        ? 'video/mp4'
        : extension === '.jpg' || extension === '.jpeg'
          ? 'image/jpeg'
          : extension === '.gif'
            ? 'image/gif'
            : extension === '.webp'
              ? 'image/webp'
              : 'image/png'
      return {
        data,
        artifacts: [{
          id: 'generated-media',
          kind: data.mediaType,
          mimeType,
          displayName: basename(data.savedPath),
          location: { type: 'runtimePath', path: data.savedPath },
        }],
      }
    },
    mapToolResultToToolResultBlockParam(output: Output, toolUseID) {
      let content: string
      if (isMediaGenerationError(output)) {
        content = output.error
      } else if (output.mediaType === 'image') {
        content = `Image generated successfully as ${basename(output.savedPath)} (model: ${output.modelId}). Revised prompt: ${output.revisedPrompt}`
      } else {
        const dur =
          output.durationSeconds != null
            ? ` (${output.durationSeconds}s)`
            : ''
        content = `Video generated successfully as ${basename(output.savedPath)}${dur} (model: ${output.modelId}).`
      }
      return {
        tool_use_id: toolUseID,
        type: 'tool_result',
        content,
        // 2026-07-04 审计 PR-8:错误输出必须带 is_error——否则 no-progress 看门狗
        // 与一轮一调计数都把失败当成功（返回式 error 此前对 loop 完全不可见）。
        ...(isMediaGenerationError(output) ? { is_error: true } : {}),
      }
    },
  } satisfies ToolDef<InputSchema, Output>)
}

/**
 * 数"本轮"内 GenerateMedia 的成功 tool_result 次数（validateInput 的一轮一调判据）。
 * 本轮 = 消息尾部自最近一条 assistant 消息（当前轮 tool_use 载体）之后；
 * 成功 = 载体中 GenerateMedia tool_use id 对应的 tool_result 且非 is_error。
 * 导出仅为单测（tests/unit/media-generation-once-per-turn.test.ts）。
 */
export function countTurnGenerateMediaSuccesses(
  messages: readonly unknown[],
): number {
  let carrierContent: unknown[] | null = null
  const trailing: unknown[] = []
  for (let i = messages.length - 1; i >= 0; i--) {
    const m = messages[i] as { type?: string; message?: { content?: unknown } }
    if (m?.type === 'assistant') {
      carrierContent = Array.isArray(m.message?.content)
        ? (m.message.content as unknown[])
        : []
      break
    }
    if (m?.type === 'user') trailing.push(m)
  }
  if (!carrierContent) return 0
  const mediaToolUseIds = new Set<string>()
  for (const block of carrierContent) {
    if (!block || typeof block !== 'object') continue
    const b = block as { type?: string; id?: string; name?: string }
    if (b.type === 'tool_use' && b.name === MEDIA_GENERATION_TOOL_NAME && b.id) {
      mediaToolUseIds.add(b.id)
    }
  }
  if (mediaToolUseIds.size === 0) return 0
  let successes = 0
  for (const m of trailing) {
    const content = (m as { message?: { content?: unknown } }).message?.content
    if (!Array.isArray(content)) continue
    for (const block of content) {
      if (!block || typeof block !== 'object') continue
      const b = block as {
        type?: string
        tool_use_id?: string
        is_error?: boolean
      }
      if (b.type !== 'tool_result') continue
      if (!b.tool_use_id || !mediaToolUseIds.has(b.tool_use_id)) continue
      if (b.is_error) continue
      successes += 1
    }
  }
  return successes
}

export const MediaGenerationTool = buildMediaGenerationTool()
