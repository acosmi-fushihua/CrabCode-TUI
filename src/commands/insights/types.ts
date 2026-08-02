import { join } from 'path'
import { getCrabCodeConfigHomeDir } from '../../utils/envUtils.js'
import { getMaxEffortModel } from '../../utils/model/model.js'

// ============================================================================
// Types
// ============================================================================

export type RemoteHostInfo = {
  name: string
  sessionCount: number
}

export type SessionMeta = {
  session_id: string
  project_path: string
  start_time: string
  duration_minutes: number
  user_message_count: number
  assistant_message_count: number
  tool_counts: Record<string, number>
  languages: Record<string, number>
  git_commits: number
  git_pushes: number
  input_tokens: number
  output_tokens: number
  first_prompt: string
  summary?: string
  // New stats
  user_interruptions: number
  user_response_times: number[]
  tool_errors: number
  tool_error_categories: Record<string, number>
  uses_task_agent: boolean
  uses_mcp: boolean
  uses_web_search: boolean
  uses_web_fetch: boolean
  // Additional stats
  lines_added: number
  lines_removed: number
  files_modified: number
  message_hours: number[]
  user_message_timestamps: string[] // ISO timestamps for multi-clauding detection
}

export type SessionFacets = {
  session_id: string
  underlying_goal: string
  goal_categories: Record<string, number>
  outcome: string
  user_satisfaction_counts: Record<string, number>
  crabcode_helpfulness: string
  session_type: string
  friction_counts: Record<string, number>
  friction_detail: string
  primary_success: string
  brief_summary: string
  user_instructions_to_crabcode?: string[]
}

export type AggregatedData = {
  total_sessions: number
  total_sessions_scanned?: number
  sessions_with_facets: number
  date_range: { start: string; end: string }
  total_messages: number
  total_duration_hours: number
  total_input_tokens: number
  total_output_tokens: number
  tool_counts: Record<string, number>
  languages: Record<string, number>
  git_commits: number
  git_pushes: number
  projects: Record<string, number>
  goal_categories: Record<string, number>
  outcomes: Record<string, number>
  satisfaction: Record<string, number>
  helpfulness: Record<string, number>
  session_types: Record<string, number>
  friction: Record<string, number>
  success: Record<string, number>
  session_summaries: Array<{
    id: string
    date: string
    summary: string
    goal?: string
  }>
  // New aggregated stats
  total_interruptions: number
  total_tool_errors: number
  tool_error_categories: Record<string, number>
  user_response_times: number[]
  median_response_time: number
  avg_response_time: number
  sessions_using_task_agent: number
  sessions_using_mcp: number
  sessions_using_web_search: number
  sessions_using_web_fetch: number
  // Additional stats from Python reference
  total_lines_added: number
  total_lines_removed: number
  total_files_modified: number
  days_active: number
  messages_per_day: number
  message_hours: number[] // Hour of day for each user message (for time of day chart)
  // Multi-clauding stats (matching Python reference)
  multi_clauding: {
    overlap_events: number
    sessions_involved: number
    user_messages_during: number
  }
}

export type InsightSection = {
  name: string
  prompt: string
  maxTokens: number
}

export type InsightResults = {
  at_a_glance?: {
    whats_working?: string
    whats_hindering?: string
    quick_wins?: string
    ambitious_workflows?: string
  }
  project_areas?: {
    areas?: Array<{ name: string; session_count: number; description: string }>
  }
  interaction_style?: {
    narrative?: string
    key_pattern?: string
  }
  what_works?: {
    intro?: string
    impressive_workflows?: Array<{ title: string; description: string }>
  }
  friction_analysis?: {
    intro?: string
    categories?: Array<{
      category: string
      description: string
      examples?: string[]
    }>
  }
  suggestions?: {
    crabcode_md_additions?: Array<{
      addition: string
      why: string
      where?: string
      prompt_scaffold?: string
    }>
    features_to_try?: Array<{
      feature: string
      one_liner: string
      why_for_you: string
      example_code?: string
    }>
    usage_patterns?: Array<{
      title: string
      suggestion: string
      detail?: string
      copyable_prompt?: string
    }>
  }
  on_the_horizon?: {
    intro?: string
    opportunities?: Array<{
      title: string
      whats_possible: string
      how_to_try?: string
      copyable_prompt?: string
    }>
  }
  cc_team_improvements?: {
    improvements?: Array<{
      title: string
      detail: string
      evidence?: string
    }>
  }
  model_behavior_improvements?: {
    improvements?: Array<{
      title: string
      detail: string
      evidence?: string
    }>
  }
  fun_ending?: {
    headline?: string
    detail?: string
  }
}

/**
 * Structured export format for crabcodescope consumption
 */
export type InsightsExport = {
  metadata: {
    username: string
    generated_at: string
    crab_code_version: string
    date_range: { start: string; end: string }
    session_count: number
    remote_hosts_collected?: string[]
  }
  aggregated_data: AggregatedData
  insights: InsightResults
  facets_summary?: {
    total: number
    goal_categories: Record<string, number>
    outcomes: Record<string, number>
    satisfaction: Record<string, number>
    friction: Record<string, number>
  }
}

export type LiteSessionInfo = {
  sessionId: string
  path: string
  mtime: number
  size: number
}

// ============================================================================
// Constants
// ============================================================================

export const EXTENSION_TO_LANGUAGE: Record<string, string> = {
  '.ts': 'TypeScript',
  '.tsx': 'TypeScript',
  '.js': 'JavaScript',
  '.jsx': 'JavaScript',
  '.py': 'Python',
  '.rb': 'Ruby',
  '.go': 'Go',
  '.rs': 'Rust',
  '.java': 'Java',
  '.md': 'Markdown',
  '.json': 'JSON',
  '.yaml': 'YAML',
  '.yml': 'YAML',
  '.sh': 'Shell',
  '.css': 'CSS',
  '.html': 'HTML',
}

// Label map for cleaning up category names (matching Python reference)
export const LABEL_MAP: Record<string, string> = {
  // Goal categories
  debug_investigate: 'Debug/Investigate',
  implement_feature: 'Implement Feature',
  fix_bug: 'Fix Bug',
  write_script_tool: 'Write Script/Tool',
  refactor_code: 'Refactor Code',
  configure_system: 'Configure System',
  create_pr_commit: 'Create PR/Commit',
  analyze_data: 'Analyze Data',
  understand_codebase: 'Understand Codebase',
  write_tests: 'Write Tests',
  write_docs: 'Write Docs',
  deploy_infra: 'Deploy/Infra',
  warmup_minimal: 'Cache Warmup',
  // Success factors
  fast_accurate_search: 'Fast/Accurate Search',
  correct_code_edits: 'Correct Code Edits',
  good_explanations: 'Good Explanations',
  proactive_help: 'Proactive Help',
  multi_file_changes: 'Multi-file Changes',
  handled_complexity: 'Multi-file Changes',
  good_debugging: 'Good Debugging',
  // Friction types
  misunderstood_request: 'Misunderstood Request',
  wrong_approach: 'Wrong Approach',
  buggy_code: 'Buggy Code',
  user_rejected_action: 'User Rejected Action',
  crabcode_got_blocked: 'CrabCode Got Blocked',
  user_stopped_early: 'User Stopped Early',
  wrong_file_or_location: 'Wrong File/Location',
  excessive_changes: 'Excessive Changes',
  slow_or_verbose: 'Slow/Verbose',
  tool_failed: 'Tool Failed',
  user_unclear: 'User Unclear',
  external_issue: 'External Issue',
  // Satisfaction labels
  frustrated: 'Frustrated',
  dissatisfied: 'Dissatisfied',
  likely_satisfied: 'Likely Satisfied',
  satisfied: 'Satisfied',
  happy: 'Happy',
  unsure: 'Unsure',
  neutral: 'Neutral',
  delighted: 'Delighted',
  // Session types
  single_task: 'Single Task',
  multi_task: 'Multi Task',
  iterative_refinement: 'Iterative Refinement',
  exploration: 'Exploration',
  quick_question: 'Quick Question',
  // Outcomes
  fully_achieved: 'Fully Achieved',
  mostly_achieved: 'Mostly Achieved',
  partially_achieved: 'Partially Achieved',
  not_achieved: 'Not Achieved',
  unclear_from_transcript: 'Unclear',
  // Helpfulness
  unhelpful: 'Unhelpful',
  slightly_helpful: 'Slightly Helpful',
  moderately_helpful: 'Moderately Helpful',
  very_helpful: 'Very Helpful',
  essential: 'Essential',
}

// Fixed orderings for specific charts (matching Python reference)
export const SATISFACTION_ORDER = [
  'frustrated',
  'dissatisfied',
  'likely_satisfied',
  'satisfied',
  'happy',
  'unsure',
]

export const OUTCOME_ORDER = [
  'not_achieved',
  'partially_achieved',
  'mostly_achieved',
  'fully_achieved',
  'unclear_from_transcript',
]

export const FACET_EXTRACTION_PROMPT = `Analyze this CrabCode session and extract structured facets.

CRITICAL GUIDELINES:

1. **goal_categories**: Count ONLY what the USER explicitly asked for.
   - DO NOT count CrabCode's autonomous codebase exploration
   - DO NOT count work CrabCode decided to do on its own
   - ONLY count when user says "can you...", "please...", "I need...", "let's..."

2. **user_satisfaction_counts**: Base ONLY on explicit user signals.
   - "Yay!", "great!", "perfect!" → happy
   - "thanks", "looks good", "that works" → satisfied
   - "ok, now let's..." (continuing without complaint) → likely_satisfied
   - "that's not right", "try again" → dissatisfied
   - "this is broken", "I give up" → frustrated

3. **friction_counts**: Be specific about what went wrong.
   - misunderstood_request: CrabCode interpreted incorrectly
   - wrong_approach: Right goal, wrong solution method
   - buggy_code: Code didn't work correctly
   - user_rejected_action: User said no/stop to a tool call
   - excessive_changes: Over-engineered or changed too much

4. If very short or just warmup, use warmup_minimal for goal_category

SESSION:
`

export const SUMMARIZE_CHUNK_PROMPT = `Summarize this portion of a CrabCode session transcript. Focus on:
1. What the user asked for
2. What CrabCode did (tools used, files modified)
3. Any friction or issues
4. The outcome

Keep it concise - 3-5 sentences. Preserve specific details like file names, error messages, and user feedback.

TRANSCRIPT CHUNK:
`

// ============================================================================
// Helper Functions
// ============================================================================

// Lazy getters: getCrabCodeConfigHomeDir() is memoized and reads process.env.
// Calling it at module scope would populate the memoize cache before
// entrypoints can set CRABCODE_CONFIG_DIR, breaking all 150+ other callers.
export function getDataDir(): string {
  return join(getCrabCodeConfigHomeDir(), 'usage-data')
}
export function getFacetsDir(): string {
  return join(getDataDir(), 'facets')
}
export function getSessionMetaDir(): string {
  return join(getDataDir(), 'session-meta')
}

// Model for facet extraction and summarization (max-effort tier — best quality)
export function getAnalysisModel(): string {
  return getMaxEffortModel()
}

// Model for narrative insights (max-effort tier — best quality)
export function getInsightsModel(): string {
  return getMaxEffortModel()
}

// ============================================================================
// Utility Functions
// ============================================================================

export function safeEntries<V>(
  obj: Record<string, V> | undefined | null,
): [string, V][] {
  return obj ? Object.entries(obj) : []
}

export function safeKeys(
  obj: Record<string, unknown> | undefined | null,
): string[] {
  return obj ? Object.keys(obj) : []
}

export function isValidSessionFacets(obj: unknown): obj is SessionFacets {
  if (!obj || typeof obj !== 'object') return false
  const o = obj as Record<string, unknown>
  return (
    typeof o.underlying_goal === 'string' &&
    typeof o.outcome === 'string' &&
    typeof o.brief_summary === 'string' &&
    o.goal_categories !== null &&
    typeof o.goal_categories === 'object' &&
    o.user_satisfaction_counts !== null &&
    typeof o.user_satisfaction_counts === 'object' &&
    o.friction_counts !== null &&
    typeof o.friction_counts === 'object'
  )
}
