/**
 * attachments-parse.ts -- Parsing and extraction utilities for attachments.
 *
 * Split from attachments.ts (3997 lines) for maintainability. Contains
 * @-mention extraction, MCP resource parsing, agent mention extraction,
 * file line range parsing, and tool success tracking.
 */
import { uniq } from './array.js'
import type { Message } from 'src/types/message.js'
import { isHumanTurn } from './messagePredicates.js'

export function extractAtMentionedFiles(content: string): string[] {
  // Extract filenames mentioned with @ symbol, including line range syntax: @file.txt#L10-20
  // Also supports quoted paths for files with spaces: @"my/file with spaces.txt"
  // Example: "foo bar @baz moo" would extract "baz"
  // Example: 'check @"my file.txt" please' would extract "my file.txt"

  // Two patterns: quoted paths and regular paths
  const quotedAtMentionRegex = /(^|\s)@"([^"]+)"/g
  const regularAtMentionRegex = /(^|\s)@([^\s]+)\b/g

  const quotedMatches: string[] = []
  const regularMatches: string[] = []

  // Extract quoted mentions first (skip agent mentions like @"code-reviewer (agent)")
  let match
  while ((match = quotedAtMentionRegex.exec(content)) !== null) {
    if (match[2] && !match[2].endsWith(' (agent)')) {
      quotedMatches.push(match[2]) // The content inside quotes
    }
  }

  // Extract regular mentions
  const regularMatchArray = content.match(regularAtMentionRegex) || []
  regularMatchArray.forEach(match => {
    const filename = match.slice(match.indexOf('@') + 1)
    // Don't include if it starts with a quote (already handled as quoted)
    if (!filename.startsWith('"')) {
      regularMatches.push(filename)
    }
  })

  // Combine and deduplicate
  return uniq([...quotedMatches, ...regularMatches])
}

export function extractMcpResourceMentions(content: string): string[] {
  // Extract MCP resources mentioned with @ symbol in format @server:uri
  // Example: "@server1:resource/path" would extract "server1:resource/path"
  const atMentionRegex = /(^|\s)@([^\s]+:[^\s]+)\b/g
  const matches = content.match(atMentionRegex) || []

  // Remove the prefix (everything before @) from each match
  return uniq(matches.map(match => match.slice(match.indexOf('@') + 1)))
}

export function extractAgentMentions(content: string): string[] {
  // Extract agent mentions in two formats:
  // 1. @agent-<agent-type> (legacy/manual typing)
  //    Example: "@agent-code-elegance-refiner" -> "agent-code-elegance-refiner"
  // 2. @"<agent-type> (agent)" (from autocomplete selection)
  //    Example: '@"code-reviewer (agent)"' -> "code-reviewer"
  // Supports colons, dots, and at-signs for plugin-scoped agents like "@agent-asana:project-status-updater"
  const results: string[] = []

  // Match quoted format: @"<type> (agent)"
  const quotedAgentRegex = /(^|\s)@"([\w:.@-]+) \(agent\)"/g
  let match
  while ((match = quotedAgentRegex.exec(content)) !== null) {
    if (match[2]) {
      results.push(match[2])
    }
  }

  // Match unquoted format: @agent-<type>
  const unquotedAgentRegex = /(^|\s)@(agent-[\w:.@-]+)/g
  const unquotedMatches = content.match(unquotedAgentRegex) || []
  for (const m of unquotedMatches) {
    results.push(m.slice(m.indexOf('@') + 1))
  }

  return uniq(results)
}

interface AtMentionedFileLines {
  filename: string
  lineStart?: number
  lineEnd?: number
}

export function parseAtMentionedFileLines(
  mention: string,
): AtMentionedFileLines {
  // Parse mentions like "file.txt#L10-20", "file.txt#heading", or just "file.txt"
  // Supports line ranges (#L10, #L10-20) and strips non-line-range fragments (#heading)
  const match = mention.match(/^([^#]+)(?:#L(\d+)(?:-(\d+))?)?(?:#[^#]*)?$/)

  if (!match) {
    return { filename: mention }
  }

  const [, filename, lineStartStr, lineEndStr] = match
  const lineStart = lineStartStr ? parseInt(lineStartStr, 10) : undefined
  const lineEnd = lineEndStr ? parseInt(lineEndStr, 10) : lineStart

  return { filename: filename ?? mention, lineStart, lineEnd }
}

type ToolResultBlock = {
  type: 'tool_result'
  tool_use_id: string
  is_error?: boolean
}

function isToolResultBlock(b: unknown): b is ToolResultBlock {
  return (
    typeof b === 'object' &&
    b !== null &&
    (b as ToolResultBlock).type === 'tool_result' &&
    typeof (b as ToolResultBlock).tool_use_id === 'string'
  )
}

/**
 * Check whether a user message's content contains tool_result blocks.
 * This is more reliable than checking `toolUseResult === undefined` because
 * sub-agent tool result messages explicitly set `toolUseResult` to `undefined`
 * when `preserveToolUseResults` is false (the default for Explore agents).
 */
export function hasToolResultContent(content: unknown): boolean {
  return Array.isArray(content) && content.some(isToolResultBlock)
}

/**
 * Tools that succeeded (and never errored) since the previous real turn
 * boundary.  The memory selector uses this to suppress docs about tools
 * that are working -- surfacing reference material for a tool the model
 * is already calling successfully is noise.
 *
 * Any error -> tool excluded (model is struggling, docs stay available).
 * No result yet -> also excluded (outcome unknown).
 *
 * tool_use lives in assistant content; tool_result in user content
 * (toolUseResult set, isMeta undefined).  Both are within the scan window.
 * Backward scan sees results before uses so we collect both by id and
 * resolve after.
 */
export function collectRecentSuccessfulTools(
  messages: ReadonlyArray<Message>,
  lastUserMessage: Message,
): readonly string[] {
  const useIdToName = new Map<string, string>()
  const resultByUseId = new Map<string, boolean>()
  for (let i = messages.length - 1; i >= 0; i--) {
    const m = messages[i]
    if (!m) continue
    if (isHumanTurn(m) && m !== lastUserMessage) break
    if (m.type === 'assistant' && typeof m.message.content !== 'string') {
      for (const block of m.message.content) {
        if (block.type === 'tool_use') useIdToName.set(block.id, block.name)
      }
    } else if (
      m.type === 'user' &&
      'message' in m &&
      Array.isArray(m.message.content)
    ) {
      for (const block of m.message.content) {
        if (isToolResultBlock(block)) {
          resultByUseId.set(block.tool_use_id, block.is_error === true)
        }
      }
    }
  }
  const failed = new Set<string>()
  const succeeded = new Set<string>()
  for (const [id, name] of useIdToName) {
    const errored = resultByUseId.get(id)
    if (errored === undefined) continue
    if (errored) {
      failed.add(name)
    } else {
      succeeded.add(name)
    }
  }
  return [...succeeded].filter(t => !failed.has(t))
}
