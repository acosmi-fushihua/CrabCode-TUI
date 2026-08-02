/**
 * Deterministic shape repair for OpenAI-compatible Chat Completions requests.
 *
 * Some providers reject optional/newer fields while others require the newer
 * token-limit spelling. We only mutate the request after a 400 response whose
 * body names the incompatible field, and each repair is one-shot.
 */

export interface OpenAIChatCompletionRepairState {
  strippedStreamOptions: boolean
  renamedMaxTokens: boolean
  strippedMetadataWithoutStore: boolean
  cappedToolsMax: number | null
}

export interface OpenAIChatCompletionRepair {
  body: Record<string, unknown>
  reason:
    | 'strip_stream_options'
    | 'rename_max_completion_tokens'
    | 'strip_metadata_without_store'
    | `cap_tools_${number}`
}

export function createOpenAIChatCompletionRepairState(): OpenAIChatCompletionRepairState {
  return {
    strippedStreamOptions: false,
    renamedMaxTokens: false,
    strippedMetadataWithoutStore: false,
    cappedToolsMax: null,
  }
}

export function maybeRepairOpenAIChatCompletionRequest(
  body: Record<string, unknown>,
  detail: string,
  state: OpenAIChatCompletionRepairState,
): OpenAIChatCompletionRepair | null {
  if (
    !state.strippedStreamOptions &&
    'stream_options' in body &&
    detail.includes('stream_options')
  ) {
    state.strippedStreamOptions = true
    const { stream_options: _dropped, ...rest } = body
    return { body: rest, reason: 'strip_stream_options' }
  }

  if (
    !state.renamedMaxTokens &&
    'max_tokens' in body &&
    detail.includes('max_completion_tokens')
  ) {
    state.renamedMaxTokens = true
    const { max_tokens: maxTokens, ...rest } = body
    return {
      body: { ...rest, max_completion_tokens: maxTokens },
      reason: 'rename_max_completion_tokens',
    }
  }

  if (
    !state.strippedMetadataWithoutStore &&
    'metadata' in body &&
    explicitlyRejectsMetadata(detail)
  ) {
    state.strippedMetadataWithoutStore = true
    const { metadata: _dropped, ...rest } = body
    return { body: rest, reason: 'strip_metadata_without_store' }
  }

  const toolsLimit = parseOpenAIToolsLimit(detail)
  if (
    toolsLimit !== null &&
    state.cappedToolsMax !== toolsLimit &&
    Array.isArray(body.tools) &&
    body.tools.length > toolsLimit
  ) {
    state.cappedToolsMax = toolsLimit
    return {
      body: capOpenAITools(body, toolsLimit),
      reason: `cap_tools_${toolsLimit}`,
    }
  }

  return null
}

/**
 * Keep metadata repair response-driven and narrow. The original OpenAI error
 * says metadata requires `store`; strict OpenAI-compatible schemas can instead
 * report the field itself as unknown (for example `Unknown name "metadata":
 * Cannot find field.`). Both are deterministic request-shape failures.
 *
 * Do not reduce this to `detail.includes('metadata')`: an unrelated 400 may
 * mention metadata diagnostically, and dropping a valid field would then hide
 * the real provider error.
 */
function explicitlyRejectsMetadata(detail: string): boolean {
  const normalized = detail.toLowerCase().replaceAll('\\"', '"')

  // Preserve the OpenAI `metadata` + `store` repair only when the provider
  // states the actual field dependency. Merely mentioning both words is not
  // enough: "failed to store ... metadata JSON is invalid" is a content
  // error, and deleting metadata would hide it behind an unsafe retry.
  const explicitlyRequiresStore = [
    /\bmetadata\b.{0,64}\b(?:is\s+)?only\s+(?:allowed|supported|available)\b.{0,64}\bstore\b.{0,32}\b(?:enabled|true)\b/,
    /\bmetadata\b.{0,64}\b(?:requires?|needs?)\b.{0,32}\bstore\b.{0,32}\b(?:enabled|true)\b/,
    /\bstore\b.{0,32}\b(?:must\s+be|needs?\s+to\s+be|is\s+required\s+to\s+be)\b.{0,16}\b(?:enabled|true)\b.{0,64}\bmetadata\b/,
  ].some(pattern => pattern.test(normalized))
  if (explicitlyRequiresStore) {
    return true
  }

  // Strict schema validators associate one of these exact error classes with
  // the metadata field. Keep the field adjacency tight to avoid matching two
  // unrelated clauses in a long provider diagnostic.
  return [
    /(?:unknown\s+(?:name|field|parameter)|unrecognized\s+(?:field|parameter)|unsupported\s+(?:field|parameter))\s*[:=-]?\s*["'`]?metadata\b/,
    /\bmetadata\b["'`]?\s*[:=-]\s*cannot\s+find\s+field\b/,
    /cannot\s+find\s+field\s*[:=-]?\s*["'`]?metadata\b/,
  ].some(pattern => pattern.test(normalized))
}

function parseOpenAIToolsLimit(detail: string): number | null {
  if (!detail.includes('tools')) return null
  const match =
    detail.match(/maximum length\s+(\d+)/i) ??
    detail.match(/maximum(?: array)?(?: length)?(?: of)?\s+(\d+)/i)
  if (!match?.[1]) return null
  const limit = Number(match[1])
  return Number.isInteger(limit) && limit > 0 ? limit : null
}

function capOpenAITools(
  body: Record<string, unknown>,
  limit: number,
): Record<string, unknown> {
  const tools = Array.isArray(body.tools) ? body.tools.slice(0, limit) : body.tools
  const next: Record<string, unknown> = { ...body, tools }
  if (!isToolChoiceAvailable(next.tool_choice, tools)) {
    next.tool_choice = 'auto'
  }
  return next
}

function isToolChoiceAvailable(toolChoice: unknown, tools: unknown): boolean {
  if (!toolChoice || typeof toolChoice !== 'object') return true
  const functionChoice = (toolChoice as Record<string, unknown>).function
  if (!functionChoice || typeof functionChoice !== 'object') return true
  const chosenName = (functionChoice as Record<string, unknown>).name
  if (typeof chosenName !== 'string') return true
  if (!Array.isArray(tools)) return false
  return tools.some(tool => {
    if (!tool || typeof tool !== 'object') return false
    const fn = (tool as Record<string, unknown>).function
    return (
      fn !== null &&
      typeof fn === 'object' &&
      (fn as Record<string, unknown>).name === chosenName
    )
  })
}
