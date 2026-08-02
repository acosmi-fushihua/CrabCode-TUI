
import type { Base64ImageSource, ContentBlockParam, MessageParam } from '../../types/api-types.js'
import type { PromptMessage, ResourceLink } from '@modelcontextprotocol/sdk/types.js'
import { basename } from 'node:path'
import type {
  ToolArtifactCandidate,
  ToolArtifactKind,
} from '../../types/toolArtifact.js'
import {
  MAX_TOOL_ARTIFACT_BYTES_BY_KIND,
  MAX_TOOL_ARTIFACTS_PER_RESULT,
  MAX_TOTAL_TOOL_ARTIFACT_BYTES_PER_RESULT,
} from '../../utils/toolArtifacts.js'
import { isEnvDefinedFalsy } from '../../utils/envUtils.js'
import {
  TelemetrySafeError_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
} from '../../utils/errors.js'
import {
  isImageWithinArtifactDecodeBudget,
  readImageArtifactSafetyMetadata,
  sniffSupportedImageMimeBytes,
} from '../../utils/imageArtifactSafety.js'
import { maybeResizeAndDownsampleImageBuffer } from '../../utils/imageResizer.js'
import { logMCPError } from '../../utils/log.js'
import {
  getBinaryBlobSavedMessage,
  getFormatDescription,
  getLargeOutputInstructions,
  persistBinaryContent,
} from '../../utils/mcpOutputStorage.js'
import {
  getContentSizeEstimate,
  type MCPToolResult,
  mcpContentNeedsTruncation,
  truncateMcpContentIfNeeded,
} from '../../utils/mcpValidation.js'
import {
  type AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
  logEvent,
} from '../analytics/index.js'
import { normalizeNameForMCP } from './normalization.js'
import {
  isPersistError,
  persistToolResult,
} from '../../utils/toolResultStorage.js'
import { jsonStringify } from '../../utils/slowOperations.js'

const IMAGE_MIME_TYPES = new Set([
  'image/jpeg',
  'image/png',
  'image/gif',
  'image/webp',
])

/**
 * Transform result content from an MCP tool or MCP prompt into message blocks
 */
export async function transformResultContent(
  resultContent: PromptMessage['content'],
  serverName: string,
): Promise<Array<ContentBlockParam>> {
  const transformed = await transformResultContentWithArtifacts(
    resultContent,
    serverName,
    0,
    true,
    false,
  )
  return transformed.content
}

/**
 * Processes MCP tool result into a normalized format.
 */
export type MCPResultType = 'toolResult' | 'structuredContent' | 'contentArray'

export type TransformedMCPResult = {
  content: MCPToolResult
  type: MCPResultType
  schema?: string
}

export type TransformedMCPResultWithArtifacts = TransformedMCPResult & {
  artifacts: ToolArtifactCandidate[]
}

export type ProcessedMCPResultWithArtifacts = {
  content: MCPToolResult
  artifacts: ToolArtifactCandidate[]
}

type TransformedContentWithArtifacts = {
  content: Array<ContentBlockParam>
  artifacts: ToolArtifactCandidate[]
}

type ResizedImagePreview = Awaited<
  ReturnType<typeof maybeResizeAndDownsampleImageBuffer>
>

type MCPArtifactWriteBudget = {
  remainingBytes: number
}

function decodeMCPBase64(
  value: unknown,
  maxBytes: number,
): Buffer | { error: string } {
  if (typeof value !== 'string') return { error: 'binary content is not base64 text' }
  // Real-world MCP servers emit RFC 2045 folded base64 (embedded whitespace)
  // and occasionally base64url; both decoded fine before this strict gate and
  // must keep decoding. Normalize the alphabet first, then apply the strict
  // shape check so genuinely invalid payloads are still rejected.
  const normalized = value
    .replace(/[\t\n\r ]/g, '')
    .replace(/-/g, '+')
    .replace(/_/g, '/')
  const maxBase64Chars = Math.ceil(maxBytes / 3) * 4
  if (normalized.length > maxBase64Chars) {
    return { error: `base64 content exceeds the ${maxBytes} byte decoded limit` }
  }
  if (
    normalized.length % 4 === 1 ||
    !/^[A-Za-z0-9+/]*={0,2}$/.test(normalized)
  ) {
    return { error: 'binary content is not valid base64' }
  }
  const bytes = Buffer.from(normalized, 'base64')
  if (bytes.length > maxBytes) {
    return { error: `binary content exceeds the ${maxBytes} byte decoded limit` }
  }
  return bytes
}

function normalizedMimeType(mimeType: string | undefined): string {
  return (mimeType?.split(';')[0] ?? 'application/octet-stream')
    .trim()
    .toLowerCase()
}

function artifactKindForMimeType(mimeType: string): ToolArtifactKind {
  if (mimeType.startsWith('image/')) return 'image'
  if (mimeType.startsWith('video/')) return 'video'
  if (mimeType.startsWith('audio/')) return 'audio'
  if (mimeType === 'application/zip') {
    return 'archive'
  }
  if (
    mimeType === 'application/pdf' ||
    mimeType.startsWith('application/vnd.') ||
    mimeType.startsWith('text/')
  ) {
    return 'document'
  }
  return 'other'
}

function mimeTypeForResizedImage(resized: ResizedImagePreview): string {
  const mediaType = resized.mediaType.toLowerCase()
  if (mediaType.startsWith('image/')) return mediaType
  return `image/${mediaType === 'jpg' ? 'jpeg' : mediaType}`
}

async function resizeMCPImagePreview(
  bytes: Buffer,
  ext: string,
  requiredForModelContent: boolean,
): Promise<ResizedImagePreview | null> {
  try {
    // Reject crafted headers (huge IHDR/SOF dimensions in a tiny payload)
    // before sharp allocates decode buffers. Gate ONLY images whose container
    // metadata strictly parses and exceeds the budget: loosely structured but
    // sharp-decodable images keep the legacy path (bounded by sharp's own
    // input-pixel limit), so no previously-working server output regresses.
    const sniffed = sniffSupportedImageMimeBytes(bytes)
    if (
      sniffed !== null &&
      readImageArtifactSafetyMetadata(bytes, sniffed) !== null &&
      !isImageWithinArtifactDecodeBudget(bytes, sniffed)
    ) {
      throw new Error('image content exceeds the artifact decode budget')
    }
    return await maybeResizeAndDownsampleImageBuffer(bytes, bytes.length, ext)
  } catch (error) {
    // Preserve the legacy model-content contract: if the image itself is the
    // MCP result, a conversion failure still rejects that invalid model block.
    // When structuredContent/toolResult won the existing priority decision,
    // image conversion is display-only and must fail soft.
    if (requiredForModelContent) throw error
    return null
  }
}

function displayNameFromUri(uri: string, fallback: string): string {
  try {
    return basename(new URL(uri).pathname) || fallback
  } catch {
    return basename(uri) || fallback
  }
}

async function persistMCPArtifact(
  bytes: Buffer,
  mimeType: string | undefined,
  serverName: string,
  contentIndex: number,
  displayName: string,
  collectAsArtifact: boolean,
  writeBudget: MCPArtifactWriteBudget,
): Promise<
  | { filepath: string; size: number; artifact: ToolArtifactCandidate }
  | { error: string }
> {
  const normalizedMime = normalizedMimeType(mimeType)
  const kind = artifactKindForMimeType(normalizedMime)
  if (bytes.length > MAX_TOOL_ARTIFACT_BYTES_BY_KIND[kind]) {
    return {
      error: `binary content exceeds the ${MAX_TOOL_ARTIFACT_BYTES_BY_KIND[kind]} byte ${kind} artifact limit`,
    }
  }
  if (bytes.length > writeBudget.remainingBytes) {
    return {
      error: `binary content exceeds the remaining ${writeBudget.remainingBytes} byte per-result persistence budget`,
    }
  }
  const persistId = collectAsArtifact
    ? `mcp-${normalizeNameForMCP(serverName)}-artifact-${contentIndex}-${Date.now()}-${Math.random().toString(36).slice(2, 8)}`
    : `mcp-${normalizeNameForMCP(serverName)}-blob-${Date.now()}-${Math.random().toString(36).slice(2, 8)}`
  const result = await persistBinaryContent(bytes, mimeType, persistId)
  if ('error' in result) return result
  writeBudget.remainingBytes -= result.size

  return {
    ...result,
    artifact: {
      id: basename(result.filepath),
      kind,
      mimeType: normalizedMime,
      displayName,
      location: { type: 'runtimePath', path: result.filepath },
      byteSize: result.size,
    },
  }
}

async function transformResultContentWithArtifacts(
  resultContent: PromptMessage['content'],
  serverName: string,
  contentIndex: number,
  includeModelContent: boolean,
  collectArtifacts = true,
  writeBudget: MCPArtifactWriteBudget = {
    remainingBytes: MAX_TOTAL_TOOL_ARTIFACT_BYTES_PER_RESULT,
  },
): Promise<TransformedContentWithArtifacts> {
  switch (resultContent.type) {
    case 'text':
      return {
        content: includeModelContent
          ? [{ type: 'text', text: resultContent.text }]
          : [],
        artifacts: [],
      }
    case 'image': {
      const decoded = decodeMCPBase64(
        resultContent.data,
        MAX_TOOL_ARTIFACT_BYTES_BY_KIND.image,
      )
      if ('error' in decoded) {
        return {
          content: includeModelContent
            ? [{ type: 'text', text: `[Image from ${serverName}] ${decoded.error}` }]
            : [],
          artifacts: [],
        }
      }
      const imageBuffer = decoded
      const ext = resultContent.mimeType?.split('/')[1] || 'png'
      const resized = await resizeMCPImagePreview(
        imageBuffer,
        ext,
        includeModelContent,
      )
      if (resized === null) return { content: [], artifacts: [] }
      const previewMimeType = mimeTypeForResizedImage(resized)
      const content: Array<ContentBlockParam> = []
      if (includeModelContent) {
        content.push({
          type: 'image',
          source: {
            data: resized.buffer.toString('base64'),
            media_type: previewMimeType as Base64ImageSource['media_type'],
            type: 'base64',
          },
        })
      }
      const persisted = collectArtifacts
        ? await persistMCPArtifact(
            resized.buffer,
            previewMimeType,
            serverName,
            contentIndex,
            `${normalizeNameForMCP(serverName)}-image.${previewMimeType.slice(6)}`,
            true,
            writeBudget,
          )
        : null
      return {
        content,
        artifacts:
          persisted === null || 'error' in persisted
            ? []
            : [persisted.artifact],
      }
    }
    case 'audio': {
      const decoded = decodeMCPBase64(
        resultContent.data,
        MAX_TOOL_ARTIFACT_BYTES_BY_KIND.audio,
      )
      if ('error' in decoded) {
        return {
          content: includeModelContent
            ? [{ type: 'text', text: `[Audio from ${serverName}] ${decoded.error}` }]
            : [],
          artifacts: [],
        }
      }
      const bytes = decoded
      const prefix = `[Audio from ${serverName}] `
      const persisted = await persistMCPArtifact(
        bytes,
        resultContent.mimeType,
        serverName,
        contentIndex,
        `${normalizeNameForMCP(serverName)}-audio.${
          resultContent.mimeType?.split('/')[1] || 'bin'
        }`,
        collectArtifacts,
        writeBudget,
      )
      if ('error' in persisted) {
        return {
          content: includeModelContent ? [
            {
              type: 'text',
              text: `${prefix}Binary content (${resultContent.mimeType || 'unknown type'}, ${bytes.length} bytes) could not be saved to disk: ${persisted.error}`,
            },
          ] : [],
          artifacts: [],
        }
      }
      return {
        content: includeModelContent ? [
          {
            type: 'text',
            text: getBinaryBlobSavedMessage(
              persisted.filepath,
              resultContent.mimeType,
              persisted.size,
              prefix,
            ),
          },
        ] : [],
        artifacts: collectArtifacts ? [persisted.artifact] : [],
      }
    }
    case 'resource': {
      const resource = resultContent.resource
      const prefix = `[Resource from ${serverName} at ${resource.uri}] `
      if ('text' in resource) {
        return {
          content: includeModelContent
            ? [{ type: 'text', text: `${prefix}${resource.text}` }]
            : [],
          artifacts: [],
        }
      }

      const isImage = IMAGE_MIME_TYPES.has(resource.mimeType ?? '')
      const resourceKind = artifactKindForMimeType(
        normalizedMimeType(resource.mimeType),
      )
      const decoded = decodeMCPBase64(
        resource.blob,
        MAX_TOOL_ARTIFACT_BYTES_BY_KIND[resourceKind],
      )
      if ('error' in decoded) {
        return {
          content: includeModelContent
            ? [{ type: 'text', text: `${prefix}${decoded.error}` }]
            : [],
          artifacts: [],
        }
      }
      const bytes = decoded
      let imageContent: MessageParam['content'] | null = null
      let artifactBytes: Buffer = bytes
      let artifactMimeType = resource.mimeType
      if (isImage) {
        const ext = resource.mimeType?.split('/')[1] || 'png'
        const resized = await resizeMCPImagePreview(
          bytes,
          ext,
          includeModelContent,
        )
        if (resized === null) return { content: [], artifacts: [] }
        artifactBytes = resized.buffer
        artifactMimeType = mimeTypeForResizedImage(resized)
        if (includeModelContent) {
          imageContent = [
            { type: 'text', text: prefix },
            {
              type: 'image',
              source: {
                data: resized.buffer.toString('base64'),
                media_type:
                  artifactMimeType as Base64ImageSource['media_type'],
                type: 'base64',
              },
            },
          ]
        }
      }

      const persisted = isImage && !collectArtifacts
        ? null
        : await persistMCPArtifact(
            artifactBytes,
            artifactMimeType,
            serverName,
            contentIndex,
            displayNameFromUri(resource.uri, `resource-${contentIndex}`),
            collectArtifacts,
            writeBudget,
          )
      if (persisted === null) {
        return { content: imageContent ?? [], artifacts: [] }
      }
      if ('error' in persisted) {
        return {
          content:
            !includeModelContent ? [] : imageContent ??
            [
              {
                type: 'text',
                text: `${prefix}Binary content (${resource.mimeType || 'unknown type'}, ${bytes.length} bytes) could not be saved to disk: ${persisted.error}`,
              },
            ],
          artifacts: [],
        }
      }
      return {
        content:
          !includeModelContent ? [] : imageContent ??
          [
            {
              type: 'text',
              text: getBinaryBlobSavedMessage(
                persisted.filepath,
                resource.mimeType,
                persisted.size,
                prefix,
              ),
            },
          ],
        artifacts: collectArtifacts ? [persisted.artifact] : [],
      }
    }
    case 'resource_link': {
      const resourceLink = resultContent as ResourceLink
      let text = `[Resource link: ${resourceLink.name}] ${resourceLink.uri}`
      if (resourceLink.description) text += ` (${resourceLink.description})`
      const content: Array<ContentBlockParam> = includeModelContent
        ? [{ type: 'text', text }]
        : []
      let url: URL
      try {
        url = new URL(resourceLink.uri)
      } catch {
        return { content, artifacts: [] }
      }
      if (url.protocol !== 'https:' && url.protocol !== 'http:') {
        return { content, artifacts: [] }
      }
      const mimeType = normalizedMimeType(resourceLink.mimeType)
      if (!collectArtifacts) return { content, artifacts: [] }
      return {
        content,
        artifacts: [
          {
            id: `mcp-resource-link-${contentIndex}`,
            kind: artifactKindForMimeType(mimeType),
            mimeType,
            displayName:
              resourceLink.title || resourceLink.name ||
              displayNameFromUri(resourceLink.uri, `resource-${contentIndex}`),
            location: { type: 'externalUri', uri: resourceLink.uri },
            ...(typeof resourceLink.size === 'number' &&
            Number.isSafeInteger(resourceLink.size) &&
            resourceLink.size >= 0
              ? { byteSize: resourceLink.size }
              : {}),
          },
        ],
      }
    }
    default:
      return { content: [], artifacts: [] }
  }
}

/**
 * Generates a compact, jq-friendly type signature for a value.
 * e.g. "{title: string, items: [{id: number, name: string}]}"
 */
export function inferCompactSchema(value: unknown, depth = 2): string {
  if (value === null) return 'null'
  if (Array.isArray(value)) {
    if (value.length === 0) return '[]'
    return `[${inferCompactSchema(value[0], depth - 1)}]`
  }
  if (typeof value === 'object') {
    if (depth <= 0) return '{...}'
    const entries = Object.entries(value).slice(0, 10)
    const props = entries.map(
      ([k, v]) => `${k}: ${inferCompactSchema(v, depth - 1)}`,
    )
    const suffix = Object.keys(value).length > 10 ? ', ...' : ''
    return `{${props.join(', ')}${suffix}}`
  }
  return typeof value
}

export async function transformMCPResult(
  result: unknown,
  tool: string, // Tool name for validation (e.g., "search")
  name: string, // Server name for transformation (e.g., "slack")
): Promise<TransformedMCPResult> {
  if (result && typeof result === 'object') {
    if ('toolResult' in result) {
      return {
        content: String(result.toolResult),
        type: 'toolResult',
      }
    }

    if (
      'structuredContent' in result &&
      result.structuredContent !== undefined
    ) {
      return {
        content: jsonStringify(result.structuredContent),
        type: 'structuredContent',
        schema: inferCompactSchema(result.structuredContent),
      }
    }

    if ('content' in result && Array.isArray(result.content)) {
      const transformedContent: Array<ContentBlockParam> = []
      const writeBudget: MCPArtifactWriteBudget = {
        remainingBytes: MAX_TOTAL_TOOL_ARTIFACT_BYTES_PER_RESULT,
      }
      for (const [index, item] of result.content.entries()) {
        const transformed = await transformResultContentWithArtifacts(
          item,
          name,
          index,
          true,
          false,
          writeBudget,
        )
        transformedContent.push(...transformed.content)
      }
      return {
        content: transformedContent,
        type: 'contentArray',
        schema: inferCompactSchema(transformedContent),
      }
    }
  }

  const errorMsg = `MCP server "${name}" tool "${tool}": unexpected response format`
  logMCPError(name, errorMsg)
  throw new TelemetrySafeError_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS(
    errorMsg,
    'MCP tool unexpected response format',
  )
}

/**
 * Artifact-aware MCP transformation. Standard binary `content` blocks are
 * transformed once so the model payload and display-artifact projection share
 * the same controlled persistence boundary. `structuredContent` keeps its
 * existing model priority while `content` is still inspected for standard MCP
 * artifact block types only.
 */
export async function transformMCPResultWithArtifacts(
  result: unknown,
  tool: string,
  name: string,
  options: { maxPersistedArtifactBytes?: number } = {},
): Promise<TransformedMCPResultWithArtifacts> {
  const writeBudget: MCPArtifactWriteBudget = {
    remainingBytes: Math.max(
      0,
      Math.min(
        options.maxPersistedArtifactBytes ??
          MAX_TOTAL_TOOL_ARTIFACT_BYTES_PER_RESULT,
        MAX_TOTAL_TOOL_ARTIFACT_BYTES_PER_RESULT,
      ),
    ),
  }
  let transformedContent: TransformedContentWithArtifacts | undefined
  if (
    result &&
    typeof result === 'object' &&
    'content' in result &&
    Array.isArray(result.content)
  ) {
    const includeModelContent =
      !('toolResult' in result) &&
      !(
        'structuredContent' in result &&
        result.structuredContent !== undefined
      )
    transformedContent = {
      content: [],
      artifacts: [],
    }
    // MCP content can contain many images/blobs. Process in wire order so
    // sharp and persistence never fan out into an unbounded Promise.all.
    for (const [index, item] of result.content.entries()) {
      const hasArtifactCapacity =
        transformedContent.artifacts.length < MAX_TOOL_ARTIFACTS_PER_RESULT
      if (!includeModelContent && !hasArtifactCapacity) {
        // structuredContent/toolResult already owns the model payload. Once
        // display capacity is full, the remaining standard content blocks are
        // display-only and must not consume image or persistence resources.
        break
      }
      const transformed = await transformResultContentWithArtifacts(
        item,
        name,
        index,
        includeModelContent,
        hasArtifactCapacity,
        writeBudget,
      )
      transformedContent.content.push(...transformed.content)
      const remainingArtifactCapacity =
        MAX_TOOL_ARTIFACTS_PER_RESULT - transformedContent.artifacts.length
      if (remainingArtifactCapacity > 0) {
        transformedContent.artifacts.push(
          ...transformed.artifacts.slice(0, remainingArtifactCapacity),
        )
      }
    }
  }

  if (result && typeof result === 'object') {
    const artifacts = transformedContent?.artifacts ?? []
    if ('toolResult' in result) {
      return {
        content: String(result.toolResult),
        type: 'toolResult',
        artifacts,
      }
    }
    if (
      'structuredContent' in result &&
      result.structuredContent !== undefined
    ) {
      return {
        content: jsonStringify(result.structuredContent),
        type: 'structuredContent',
        schema: inferCompactSchema(result.structuredContent),
        artifacts,
      }
    }
    if (transformedContent) {
      return {
        content: transformedContent.content,
        type: 'contentArray',
        schema: inferCompactSchema(transformedContent.content),
        artifacts,
      }
    }
  }

  const errorMsg = `MCP server "${name}" tool "${tool}": unexpected response format`
  logMCPError(name, errorMsg)
  throw new TelemetrySafeError_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS(
    errorMsg,
    'MCP tool unexpected response format',
  )
}

/**
 * Check if MCP content contains any image blocks.
 * Used to decide whether to persist to file (images should use truncation instead
 * to preserve image compression and viewability).
 */
function contentContainsImages(content: MCPToolResult): boolean {
  if (!content || typeof content === 'string') {
    return false
  }
  return content.some(block => block.type === 'image')
}

export async function processMCPResult(
  result: unknown,
  tool: string, // Tool name for validation (e.g., "search")
  name: string, // Server name for IDE check and transformation (e.g., "slack")
): Promise<MCPToolResult> {
  const transformed = await transformMCPResult(result, tool, name)
  return await processTransformedMCPResult(transformed, tool, name)
}

export async function processMCPResultWithArtifacts(
  result: unknown,
  tool: string,
  name: string,
): Promise<ProcessedMCPResultWithArtifacts> {
  const transformed = await transformMCPResultWithArtifacts(result, tool, name)
  return {
    content: await processTransformedMCPResult(transformed, tool, name),
    artifacts: transformed.artifacts,
  }
}

async function processTransformedMCPResult(
  transformed: TransformedMCPResult,
  tool: string,
  name: string,
): Promise<MCPToolResult> {
  const { content, type, schema } = transformed

  // IDE tools are not going to the model directly, so we don't need to
  // handle large output.
  if (name === 'ide') {
    return content
  }

  // Check if content needs truncation (i.e., is too large)
  if (!(await mcpContentNeedsTruncation(content))) {
    return content
  }

  const sizeEstimateTokens = getContentSizeEstimate(content)

  // If large output files feature is disabled, fall back to old truncation behavior
  if (isEnvDefinedFalsy(process.env.ENABLE_MCP_LARGE_OUTPUT_FILES)) {
    logEvent('tengu_mcp_large_result_handled', {
      outcome: 'truncated',
      reason: 'env_disabled',
      sizeEstimateTokens,
    } as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS)
    return await truncateMcpContentIfNeeded(content)
  }

  // Save large output to file and return instructions for reading it
  // Content is guaranteed to exist at this point (we checked mcpContentNeedsTruncation)
  if (!content) {
    return content
  }

  // If content contains images, fall back to truncation - persisting images as JSON
  // defeats the image compression logic and makes them non-viewable
  if (contentContainsImages(content)) {
    logEvent('tengu_mcp_large_result_handled', {
      outcome: 'truncated',
      reason: 'contains_images',
      sizeEstimateTokens,
    } as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS)
    return await truncateMcpContentIfNeeded(content)
  }

  // Generate a unique ID for the persisted file (server__tool-timestamp)
  const timestamp = Date.now()
  const persistId = `mcp-${normalizeNameForMCP(name)}-${normalizeNameForMCP(tool)}-${timestamp}`
  // Convert to string for persistence (persistToolResult expects string or specific block types)
  const contentStr =
    typeof content === 'string' ? content : jsonStringify(content, null, 2)
  const persistResult = await persistToolResult(contentStr, persistId)

  if (isPersistError(persistResult)) {
    // If file save failed, fall back to returning truncated content info
    const contentLength = contentStr.length
    logEvent('tengu_mcp_large_result_handled', {
      outcome: 'truncated',
      reason: 'persist_failed',
      sizeEstimateTokens,
    } as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS)
    return `Error: result (${contentLength.toLocaleString()} characters) exceeds maximum allowed tokens. Failed to save output to file: ${persistResult.error}. If this MCP server provides pagination or filtering tools, use them to retrieve specific portions of the data.`
  }

  logEvent('tengu_mcp_large_result_handled', {
    outcome: 'persisted',
    reason: 'file_saved',
    sizeEstimateTokens,
    persistedSizeChars: persistResult.originalSize,
  } as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS)

  const formatDescription = getFormatDescription(type, schema)
  return getLargeOutputInstructions(
    persistResult.filepath,
    persistResult.originalSize,
    formatDescription,
  )
}
