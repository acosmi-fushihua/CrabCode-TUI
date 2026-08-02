import { diffLines } from 'diff'
import { extname } from 'path'
import { queryWithModel } from '../../services/api/crabcode.js'
import {
  AGENT_TOOL_NAME,
  LEGACY_AGENT_TOOL_NAME,
} from '../../tools/AgentTool/constants.js'
import type { LogOption } from '../../types/logs.js'
import { extractTextContent } from '../../utils/messages.js'
import {
  getSessionIdFromLog,
} from '../../utils/sessionStorage.js'
import { countCharInString } from '../../utils/stringUtils.js'
import { asSystemPrompt } from '../../utils/systemPromptType.js'
import type {
  AggregatedData,
  SessionFacets,
  SessionMeta,
} from './types.js'
import {
  EXTENSION_TO_LANGUAGE,
  SUMMARIZE_CHUNK_PROMPT,
  getAnalysisModel,
  safeEntries,
  safeKeys,
} from './types.js'

// ============================================================================
// Helper Functions
// ============================================================================

function getLanguageFromPath(filePath: string): string | null {
  const ext = extname(filePath).toLowerCase()
  return EXTENSION_TO_LANGUAGE[ext] || null
}

function extractToolStats(log: LogOption): {
  toolCounts: Record<string, number>
  languages: Record<string, number>
  gitCommits: number
  gitPushes: number
  inputTokens: number
  outputTokens: number
  // New stats
  userInterruptions: number
  userResponseTimes: number[]
  toolErrors: number
  toolErrorCategories: Record<string, number>
  usesTaskAgent: boolean
  usesMcp: boolean
  usesWebSearch: boolean
  usesWebFetch: boolean
  // Additional stats
  linesAdded: number
  linesRemoved: number
  filesModified: Set<string>
  messageHours: number[]
  userMessageTimestamps: string[] // ISO timestamps for multi-clauding detection
} {
  const toolCounts: Record<string, number> = {}
  const languages: Record<string, number> = {}
  let gitCommits = 0
  let gitPushes = 0
  let inputTokens = 0
  let outputTokens = 0

  // New stats
  let userInterruptions = 0
  const userResponseTimes: number[] = []
  let toolErrors = 0
  const toolErrorCategories: Record<string, number> = {}
  let usesTaskAgent = false

  // Additional stats
  let linesAdded = 0
  let linesRemoved = 0
  const filesModified = new Set<string>()
  const messageHours: number[] = []
  const userMessageTimestamps: string[] = [] // For multi-clauding detection
  let usesMcp = false
  let usesWebSearch = false
  let usesWebFetch = false
  let lastAssistantTimestamp: string | null = null

  for (const msg of log.messages) {
    // Get message timestamp for response time calculation
    const msgTimestamp = (msg as { timestamp?: string }).timestamp

    if (msg.type === 'assistant' && msg.message) {
      // Track timestamp for response time calculation
      if (msgTimestamp) {
        lastAssistantTimestamp = msgTimestamp
      }

      const usage = (
        msg.message as {
          usage?: { input_tokens?: number; output_tokens?: number }
        }
      ).usage
      if (usage) {
        inputTokens += usage.input_tokens || 0
        outputTokens += usage.output_tokens || 0
      }

      const content = msg.message.content
      if (Array.isArray(content)) {
        for (const block of content) {
          if (block.type === 'tool_use' && 'name' in block) {
            const toolName = block.name as string
            toolCounts[toolName] = (toolCounts[toolName] || 0) + 1

            // Check for special tool usage
            if (
              toolName === AGENT_TOOL_NAME ||
              toolName === LEGACY_AGENT_TOOL_NAME
            )
              usesTaskAgent = true
            if (toolName.startsWith('mcp__')) usesMcp = true
            if (toolName === 'WebSearch') usesWebSearch = true
            if (toolName === 'WebFetch') usesWebFetch = true

            const input = (block as { input?: Record<string, unknown> }).input

            if (input) {
              const filePath = (input.file_path as string) || ''
              if (filePath) {
                const lang = getLanguageFromPath(filePath)
                if (lang) {
                  languages[lang] = (languages[lang] || 0) + 1
                }
                // Track files modified by Edit/Write tools
                if (toolName === 'Edit' || toolName === 'Write') {
                  filesModified.add(filePath)
                }
              }

              if (toolName === 'Edit') {
                const oldString = (input.old_string as string) || ''
                const newString = (input.new_string as string) || ''
                for (const change of diffLines(oldString, newString)) {
                  if (change.added) linesAdded += change.count || 0
                  if (change.removed) linesRemoved += change.count || 0
                }
              }

              // Track lines from Write tool (all added)
              if (toolName === 'Write') {
                const writeContent = (input.content as string) || ''
                if (writeContent) {
                  linesAdded += countCharInString(writeContent, '\n') + 1
                }
              }

              const command = (input.command as string) || ''
              if (command.includes('git commit')) gitCommits++
              if (command.includes('git push')) gitPushes++
            }
          }
        }
      }
    }

    // Check user messages
    if (msg.type === 'user' && msg.message) {
      const content = msg.message.content

      // Check if this is an actual human message (has text) vs just tool_result
      // matching Python reference logic
      let isHumanMessage = false
      if (typeof content === 'string' && content.trim()) {
        isHumanMessage = true
      } else if (Array.isArray(content)) {
        for (const block of content) {
          if (block.type === 'text' && 'text' in block) {
            isHumanMessage = true
            break
          }
        }
      }

      // Only track message hours and response times for actual human messages
      if (isHumanMessage) {
        // Track message hour for time-of-day analysis and timestamp for multi-clauding
        if (msgTimestamp) {
          try {
            const msgDate = new Date(msgTimestamp)
            const hour = msgDate.getHours() // Local hour 0-23
            messageHours.push(hour)
            // Collect timestamp for multi-clauding detection (matching Python)
            userMessageTimestamps.push(msgTimestamp)
          } catch {
            // Skip invalid timestamps
          }
        }

        // Calculate response time (time from last assistant message to this user message)
        // Only count gaps > 2 seconds (real user think time, not tool results)
        if (lastAssistantTimestamp && msgTimestamp) {
          const assistantTime = new Date(lastAssistantTimestamp).getTime()
          const userTime = new Date(msgTimestamp).getTime()
          const responseTimeSec = (userTime - assistantTime) / 1000
          // Only count reasonable response times (2s-1 hour) matching Python
          if (responseTimeSec > 2 && responseTimeSec < 3600) {
            userResponseTimes.push(responseTimeSec)
          }
        }
      }

      // Process tool results (for error tracking)
      if (Array.isArray(content)) {
        for (const block of content) {
          if (block.type === 'tool_result' && 'content' in block) {
            const isError = (block as { is_error?: boolean }).is_error

            // Count and categorize tool errors (matching Python reference logic)
            if (isError) {
              toolErrors++
              const resultContent = (block as { content?: string }).content
              let category = 'Other'
              if (typeof resultContent === 'string') {
                const lowerContent = resultContent.toLowerCase()
                if (lowerContent.includes('exit code')) {
                  category = 'Command Failed'
                } else if (
                  lowerContent.includes('rejected') ||
                  lowerContent.includes("doesn't want")
                ) {
                  category = 'User Rejected'
                } else if (
                  lowerContent.includes('string to replace not found') ||
                  lowerContent.includes('no changes')
                ) {
                  category = 'Edit Failed'
                } else if (lowerContent.includes('modified since read')) {
                  category = 'File Changed'
                } else if (
                  lowerContent.includes('exceeds maximum') ||
                  lowerContent.includes('too large')
                ) {
                  category = 'File Too Large'
                } else if (
                  lowerContent.includes('file not found') ||
                  lowerContent.includes('does not exist')
                ) {
                  category = 'File Not Found'
                }
              }
              toolErrorCategories[category] =
                (toolErrorCategories[category] || 0) + 1
            }
          }
        }
      }

      // Check for interruptions (matching Python reference)
      if (typeof content === 'string') {
        if (content.includes('[Request interrupted by user')) {
          userInterruptions++
        }
      } else if (Array.isArray(content)) {
        for (const block of content) {
          if (
            block.type === 'text' &&
            'text' in block &&
            (block.text as string).includes('[Request interrupted by user')
          ) {
            userInterruptions++
            break
          }
        }
      }
    }
  }

  return {
    toolCounts,
    languages,
    gitCommits,
    gitPushes,
    inputTokens,
    outputTokens,
    // New stats
    userInterruptions,
    userResponseTimes,
    toolErrors,
    toolErrorCategories,
    usesTaskAgent,
    usesMcp,
    usesWebSearch,
    usesWebFetch,
    // Additional stats
    linesAdded,
    linesRemoved,
    filesModified,
    messageHours,
    userMessageTimestamps,
  }
}

export function hasValidDates(log: LogOption): boolean {
  return (
    !Number.isNaN(log.created.getTime()) &&
    !Number.isNaN(log.modified.getTime())
  )
}

export function logToSessionMeta(log: LogOption): SessionMeta {
  const stats = extractToolStats(log)
  const sessionId = getSessionIdFromLog(log) || 'unknown'
  const startTime = log.created.toISOString()
  const durationMinutes = Math.round(
    (log.modified.getTime() - log.created.getTime()) / 1000 / 60,
  )

  let userMessageCount = 0
  let assistantMessageCount = 0
  for (const msg of log.messages) {
    if (msg.type === 'assistant') assistantMessageCount++
    // Only count user messages that have actual text content (human messages)
    // not just tool_result messages (matching Python reference)
    if (msg.type === 'user' && msg.message) {
      const content = msg.message.content
      let isHumanMessage = false
      if (typeof content === 'string' && content.trim()) {
        isHumanMessage = true
      } else if (Array.isArray(content)) {
        for (const block of content) {
          if (block.type === 'text' && 'text' in block) {
            isHumanMessage = true
            break
          }
        }
      }
      if (isHumanMessage) {
        userMessageCount++
      }
    }
  }

  return {
    session_id: sessionId,
    project_path: log.projectPath || '',
    start_time: startTime,
    duration_minutes: durationMinutes,
    user_message_count: userMessageCount,
    assistant_message_count: assistantMessageCount,
    tool_counts: stats.toolCounts,
    languages: stats.languages,
    git_commits: stats.gitCommits,
    git_pushes: stats.gitPushes,
    input_tokens: stats.inputTokens,
    output_tokens: stats.outputTokens,
    first_prompt: log.firstPrompt || '',
    summary: log.summary,
    // New stats
    user_interruptions: stats.userInterruptions,
    user_response_times: stats.userResponseTimes,
    tool_errors: stats.toolErrors,
    tool_error_categories: stats.toolErrorCategories,
    uses_task_agent: stats.usesTaskAgent,
    uses_mcp: stats.usesMcp,
    uses_web_search: stats.usesWebSearch,
    uses_web_fetch: stats.usesWebFetch,
    // Additional stats
    lines_added: stats.linesAdded,
    lines_removed: stats.linesRemoved,
    files_modified: stats.filesModified.size,
    message_hours: stats.messageHours,
    user_message_timestamps: stats.userMessageTimestamps,
  }
}

/**
 * Deduplicate conversation branches within the same session.
 *
 * When a session file has multiple leaf messages (from retries or branching),
 * loadAllLogsFromSessionFile produces one LogOption per leaf. Each branch
 * shares the same root message, so its duration overlaps with sibling
 * branches. This keeps only the branch with the most user messages
 * (tie-break by longest duration) per session_id.
 */
export function deduplicateSessionBranches(
  entries: Array<{ log: LogOption; meta: SessionMeta }>,
): Array<{ log: LogOption; meta: SessionMeta }> {
  const bestBySession = new Map<string, { log: LogOption; meta: SessionMeta }>()
  for (const entry of entries) {
    const id = entry.meta.session_id
    const existing = bestBySession.get(id)
    if (
      !existing ||
      entry.meta.user_message_count > existing.meta.user_message_count ||
      (entry.meta.user_message_count === existing.meta.user_message_count &&
        entry.meta.duration_minutes > existing.meta.duration_minutes)
    ) {
      bestBySession.set(id, entry)
    }
  }
  return [...bestBySession.values()]
}

// ============================================================================
// Transcript Formatting
// ============================================================================

export function formatTranscriptForFacets(log: LogOption): string {
  const lines: string[] = []
  const meta = logToSessionMeta(log)

  lines.push(`Session: ${meta.session_id.slice(0, 8)}`)
  lines.push(`Date: ${meta.start_time}`)
  lines.push(`Project: ${meta.project_path}`)
  lines.push(`Duration: ${meta.duration_minutes} min`)
  lines.push('')

  for (const msg of log.messages) {
    if (msg.type === 'user' && msg.message) {
      const content = msg.message.content
      if (typeof content === 'string') {
        lines.push(`[User]: ${content.slice(0, 500)}`)
      } else if (Array.isArray(content)) {
        for (const block of content) {
          if (block.type === 'text' && 'text' in block) {
            lines.push(`[User]: ${(block.text as string).slice(0, 500)}`)
          }
        }
      }
    } else if (msg.type === 'assistant' && msg.message) {
      const content = msg.message.content
      if (Array.isArray(content)) {
        for (const block of content) {
          if (block.type === 'text' && 'text' in block) {
            lines.push(`[Assistant]: ${(block.text as string).slice(0, 300)}`)
          } else if (block.type === 'tool_use' && 'name' in block) {
            lines.push(`[Tool: ${block.name}]`)
          }
        }
      }
    }
  }

  return lines.join('\n')
}

async function summarizeTranscriptChunk(chunk: string): Promise<string> {
  try {
    const result = await queryWithModel({
      systemPrompt: asSystemPrompt([]),
      userPrompt: SUMMARIZE_CHUNK_PROMPT + chunk,
      signal: new AbortController().signal,
      options: {
        model: getAnalysisModel(),
        querySource: 'insights',
        agents: [],
        isNonInteractiveSession: true,
        hasAppendSystemPrompt: false,
        mcpTools: [],
        maxOutputTokensOverride: 500,
      },
    })

    const text = extractTextContent(result.message.content)
    return text || chunk.slice(0, 2000)
  } catch {
    // On error, just return truncated chunk
    return chunk.slice(0, 2000)
  }
}

export async function formatTranscriptWithSummarization(
  log: LogOption,
): Promise<string> {
  const fullTranscript = formatTranscriptForFacets(log)

  // If under 30k chars, use as-is
  if (fullTranscript.length <= 30000) {
    return fullTranscript
  }

  // For long transcripts, split into chunks and summarize in parallel
  const CHUNK_SIZE = 25000
  const chunks: string[] = []

  for (let i = 0; i < fullTranscript.length; i += CHUNK_SIZE) {
    chunks.push(fullTranscript.slice(i, i + CHUNK_SIZE))
  }

  // Summarize all chunks in parallel
  const summaries = await Promise.all(chunks.map(summarizeTranscriptChunk))

  // Combine summaries with session header
  const meta = logToSessionMeta(log)
  const header = [
    `Session: ${meta.session_id.slice(0, 8)}`,
    `Date: ${meta.start_time}`,
    `Project: ${meta.project_path}`,
    `Duration: ${meta.duration_minutes} min`,
    `[Long session - ${chunks.length} parts summarized]`,
    '',
  ].join('\n')

  return header + summaries.join('\n\n---\n\n')
}

// ============================================================================
// Multi-clauding Detection
// ============================================================================

/**
 * Detects multi-clauding (using multiple CrabCode sessions concurrently).
 * Uses a sliding window to find the pattern: session1 -> session2 -> session1
 * within a 30-minute window.
 */
export function detectMultiClauding(
  sessions: Array<{
    session_id: string
    user_message_timestamps: string[]
  }>,
): {
  overlap_events: number
  sessions_involved: number
  user_messages_during: number
} {
  const OVERLAP_WINDOW_MS = 30 * 60000
  const allSessionMessages: Array<{ ts: number; sessionId: string }> = []

  for (const session of sessions) {
    for (const timestamp of session.user_message_timestamps) {
      try {
        const ts = new Date(timestamp).getTime()
        allSessionMessages.push({ ts, sessionId: session.session_id })
      } catch {
        // Skip invalid timestamps
      }
    }
  }

  allSessionMessages.sort((a, b) => a.ts - b.ts)

  const multiAgentSessionPairs = new Set<string>()
  const messagesDuringMultiAgent = new Set<string>()

  // Sliding window: sessionLastIndex tracks the most recent index for each session
  let windowStart = 0
  const sessionLastIndex = new Map<string, number>()

  for (let i = 0; i < allSessionMessages.length; i++) {
    const msg = allSessionMessages[i]!

    // Shrink window from the left
    while (
      windowStart < i &&
      msg.ts - allSessionMessages[windowStart]!.ts > OVERLAP_WINDOW_MS
    ) {
      const expiring = allSessionMessages[windowStart]!
      if (sessionLastIndex.get(expiring.sessionId) === windowStart) {
        sessionLastIndex.delete(expiring.sessionId)
      }
      windowStart++
    }

    // Check if this session appeared earlier in the window (pattern: s1 -> s2 -> s1)
    const prevIndex = sessionLastIndex.get(msg.sessionId)
    if (prevIndex !== undefined) {
      for (let j = prevIndex + 1; j < i; j++) {
        const between = allSessionMessages[j]!
        if (between.sessionId !== msg.sessionId) {
          const pair = [msg.sessionId, between.sessionId].sort().join(':')
          multiAgentSessionPairs.add(pair)
          messagesDuringMultiAgent.add(
            `${allSessionMessages[prevIndex]!.ts}:${msg.sessionId}`,
          )
          messagesDuringMultiAgent.add(`${between.ts}:${between.sessionId}`)
          messagesDuringMultiAgent.add(`${msg.ts}:${msg.sessionId}`)
          break
        }
      }
    }

    sessionLastIndex.set(msg.sessionId, i)
  }

  const sessionsWithOverlaps = new Set<string>()
  for (const pair of multiAgentSessionPairs) {
    const [s1, s2] = pair.split(':')
    if (s1) sessionsWithOverlaps.add(s1)
    if (s2) sessionsWithOverlaps.add(s2)
  }

  return {
    overlap_events: multiAgentSessionPairs.size,
    sessions_involved: sessionsWithOverlaps.size,
    user_messages_during: messagesDuringMultiAgent.size,
  }
}

// ============================================================================
// Aggregation
// ============================================================================

export function aggregateData(
  sessions: SessionMeta[],
  facets: Map<string, SessionFacets>,
): AggregatedData {
  const result: AggregatedData = {
    total_sessions: sessions.length,
    sessions_with_facets: facets.size,
    date_range: { start: '', end: '' },
    total_messages: 0,
    total_duration_hours: 0,
    total_input_tokens: 0,
    total_output_tokens: 0,
    tool_counts: {},
    languages: {},
    git_commits: 0,
    git_pushes: 0,
    projects: {},
    goal_categories: {},
    outcomes: {},
    satisfaction: {},
    helpfulness: {},
    session_types: {},
    friction: {},
    success: {},
    session_summaries: [],
    // New stats
    total_interruptions: 0,
    total_tool_errors: 0,
    tool_error_categories: {},
    user_response_times: [],
    median_response_time: 0,
    avg_response_time: 0,
    sessions_using_task_agent: 0,
    sessions_using_mcp: 0,
    sessions_using_web_search: 0,
    sessions_using_web_fetch: 0,
    // Additional stats
    total_lines_added: 0,
    total_lines_removed: 0,
    total_files_modified: 0,
    days_active: 0,
    messages_per_day: 0,
    message_hours: [],
    // Multi-clauding stats (matching Python reference)
    multi_clauding: {
      overlap_events: 0,
      sessions_involved: 0,
      user_messages_during: 0,
    },
  }

  const dates: string[] = []
  const allResponseTimes: number[] = []
  const allMessageHours: number[] = []

  for (const session of sessions) {
    dates.push(session.start_time)
    result.total_messages += session.user_message_count
    result.total_duration_hours += session.duration_minutes / 60
    result.total_input_tokens += session.input_tokens
    result.total_output_tokens += session.output_tokens
    result.git_commits += session.git_commits
    result.git_pushes += session.git_pushes

    // New stats aggregation
    result.total_interruptions += session.user_interruptions
    result.total_tool_errors += session.tool_errors
    for (const [cat, count] of Object.entries(session.tool_error_categories)) {
      result.tool_error_categories[cat] =
        (result.tool_error_categories[cat] || 0) + count
    }
    allResponseTimes.push(...session.user_response_times)
    if (session.uses_task_agent) result.sessions_using_task_agent++
    if (session.uses_mcp) result.sessions_using_mcp++
    if (session.uses_web_search) result.sessions_using_web_search++
    if (session.uses_web_fetch) result.sessions_using_web_fetch++

    // Additional stats aggregation
    result.total_lines_added += session.lines_added
    result.total_lines_removed += session.lines_removed
    result.total_files_modified += session.files_modified
    allMessageHours.push(...session.message_hours)

    for (const [tool, count] of Object.entries(session.tool_counts)) {
      result.tool_counts[tool] = (result.tool_counts[tool] || 0) + count
    }

    for (const [lang, count] of Object.entries(session.languages)) {
      result.languages[lang] = (result.languages[lang] || 0) + count
    }

    if (session.project_path) {
      result.projects[session.project_path] =
        (result.projects[session.project_path] || 0) + 1
    }

    const sessionFacets = facets.get(session.session_id)
    if (sessionFacets) {
      // Goal categories
      for (const [cat, count] of safeEntries(sessionFacets.goal_categories)) {
        if (count > 0) {
          result.goal_categories[cat] =
            (result.goal_categories[cat] || 0) + count
        }
      }

      // Outcomes
      result.outcomes[sessionFacets.outcome] =
        (result.outcomes[sessionFacets.outcome] || 0) + 1

      // Satisfaction counts
      for (const [level, count] of safeEntries(
        sessionFacets.user_satisfaction_counts,
      )) {
        if (count > 0) {
          result.satisfaction[level] = (result.satisfaction[level] || 0) + count
        }
      }

      // Helpfulness
      result.helpfulness[sessionFacets.crabcode_helpfulness] =
        (result.helpfulness[sessionFacets.crabcode_helpfulness] || 0) + 1

      // Session types
      result.session_types[sessionFacets.session_type] =
        (result.session_types[sessionFacets.session_type] || 0) + 1

      // Friction counts
      for (const [type, count] of safeEntries(sessionFacets.friction_counts)) {
        if (count > 0) {
          result.friction[type] = (result.friction[type] || 0) + count
        }
      }

      // Success factors
      if (sessionFacets.primary_success !== 'none') {
        result.success[sessionFacets.primary_success] =
          (result.success[sessionFacets.primary_success] || 0) + 1
      }
    }

    if (result.session_summaries.length < 50) {
      result.session_summaries.push({
        id: session.session_id.slice(0, 8),
        date: session.start_time.split('T')[0] || '',
        summary: session.summary || session.first_prompt.slice(0, 100),
        goal: sessionFacets?.underlying_goal,
      })
    }
  }

  dates.sort()
  result.date_range.start = dates[0]?.split('T')[0] || ''
  result.date_range.end = dates[dates.length - 1]?.split('T')[0] || ''

  // Calculate response time stats
  result.user_response_times = allResponseTimes
  if (allResponseTimes.length > 0) {
    const sorted = [...allResponseTimes].sort((a, b) => a - b)
    result.median_response_time = sorted[Math.floor(sorted.length / 2)] || 0
    result.avg_response_time =
      allResponseTimes.reduce((a, b) => a + b, 0) / allResponseTimes.length
  }

  // Calculate days active and messages per day
  const uniqueDays = new Set(dates.map(d => d.split('T')[0]))
  result.days_active = uniqueDays.size
  result.messages_per_day =
    result.days_active > 0
      ? Math.round((result.total_messages / result.days_active) * 10) / 10
      : 0

  // Store message hours for time-of-day chart
  result.message_hours = allMessageHours

  result.multi_clauding = detectMultiClauding(sessions)

  return result
}
