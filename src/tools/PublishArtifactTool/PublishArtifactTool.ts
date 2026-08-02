import { basename, isAbsolute } from 'node:path'
import { z } from 'zod/v4'

import { buildTool, type ToolDef, type ValidationResult } from '../../Tool.js'
import type { PermissionDecision } from '../../types/permissions.js'
import type { ToolArtifactKind } from '../../types/toolArtifact.js'
import { lazySchema } from '../../utils/lazySchema.js'
import { checkReadPermissionForTool } from '../../utils/permissions/filesystem.js'
import { PUBLISH_ARTIFACT_TOOL_NAME } from './constants.js'
import { DESCRIPTION, PROMPT } from './prompt.js'
import { createToolPresentationDelegates } from '../toolPresentationRegistry.js'

const ARTIFACT_KINDS = [
  'image',
  'video',
  'audio',
  'document',
  'archive',
  'other',
] as const satisfies readonly ToolArtifactKind[]

const inputSchema = lazySchema(() =>
  z.strictObject({
    path: z
      .string()
      .min(1)
      .describe('Absolute path of the existing file in the tool runtime.'),
    kind: z.enum(ARTIFACT_KINDS).describe('How the artifact should be displayed.'),
    mimeType: z
      .string()
      .trim()
      .min(1)
      .describe('Declared MIME type, for example video/mp4 or image/png.'),
    displayName: z
      .string()
      .trim()
      .min(1)
      .max(255)
      .optional()
      .describe('Optional user-facing filename or label.'),
  }),
)
type InputSchema = ReturnType<typeof inputSchema>

const outputSchema = lazySchema(() =>
  z.object({
    submitted: z.literal(true),
    displayName: z.string(),
  }),
)
type OutputSchema = ReturnType<typeof outputSchema>
export type Output = z.infer<OutputSchema>

/**
 * Explicitly registers one already-created file with the artifact projection
 * pipeline. This tool intentionally performs no filesystem validation itself:
 * `toolExecution` owns the single normalizeToolArtifacts boundary for every
 * producer, so Bash/skills cannot acquire a weaker validation path here.
 */
export const PublishArtifactTool = buildTool({
  name: PUBLISH_ARTIFACT_TOOL_NAME,
  searchHint: 'display a generated file inline',
  maxResultSizeChars: 4_000,
  async description() {
    return DESCRIPTION
  },
  async prompt() {
    return PROMPT
  },
  get inputSchema(): InputSchema {
    return inputSchema()
  },
  get outputSchema(): OutputSchema {
    return outputSchema()
  },
  isConcurrencySafe() {
    return true
  },
  isReadOnly() {
    return true
  },
  isDestructive() {
    return false
  },
  getPath({ path }) {
    return path
  },
  toAutoClassifierInput(input) {
    return input.path
  },
  async validateInput({ path }): Promise<ValidationResult> {
    if (!isAbsolute(path)) {
      return {
        result: false,
        message: 'PublishArtifact path must be absolute.',
        errorCode: 1,
      }
    }
    return { result: true }
  },
  async checkPermissions(input, context): Promise<PermissionDecision> {
    return checkReadPermissionForTool(
      PublishArtifactTool,
      input,
      context.getAppState().toolPermissionContext,
    )
  },
  userFacingName() {
    return 'Publish artifact'
  },
  getActivityDescription(input) {
    const name = input?.displayName || (input?.path ? basename(input.path) : '')
    return name ? `Publishing ${name}` : 'Publishing artifact'
  },
  renderToolUseMessage(input) {
    const name = input.displayName || (input.path ? basename(input.path) : '')
    return name ? `Publishing ${name}` : 'Publishing artifact'
  },
  ...createToolPresentationDelegates(PUBLISH_ARTIFACT_TOOL_NAME, [
    'renderToolResultMessage',
  ] as const),
  async call({ path, kind, mimeType, displayName }) {
    const resolvedDisplayName = displayName || basename(path) || 'artifact'
    return {
      data: {
        submitted: true as const,
        displayName: resolvedDisplayName,
      },
      artifacts: [
        {
          // Candidate ids are scoped by producerToolUseId downstream, so a
          // stable single-item id is replay-safe and sufficient here.
          id: 'published-artifact',
          kind,
          mimeType,
          displayName: resolvedDisplayName,
          location: { type: 'runtimePath' as const, path },
        },
      ],
    }
  },
  mapToolResultToToolResultBlockParam(content, toolUseID) {
    return {
      tool_use_id: toolUseID,
      type: 'tool_result',
      content: `Artifact candidate submitted for host validation: ${content.displayName}`,
    }
  },
} satisfies ToolDef<InputSchema, Output>)
