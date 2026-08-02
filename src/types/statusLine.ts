/**
 * Types for the StatusLine hook command input.
 *
 * StatusLineCommandInput is passed to StatusLine hooks so external tools
 * can render a custom status bar.
 */

export type StatusLineCommandInput = {
  /** Base hook input fields (session_id, hook_event_name, etc.) */
  session_id?: string
  hook_event_name?: string

  /** Optional session name/title */
  session_name?: string

  /** Active model info */
  model: {
    id: string
    display_name: string
  }

  /** Workspace/directory info */
  workspace: {
    current_dir: string
    project_dir: string
    added_dirs: string[]
  }

  /** CLI version string */
  version: string

  /** Active output style */
  output_style: {
    name: string
  }

  /** Cost and timing stats for the current session */
  cost: {
    total_cost_usd: number
    total_duration_ms: number
    total_api_duration_ms: number
    total_lines_added: number
    total_lines_removed: number
  }

  /** Context window usage */
  context_window: {
    total_input_tokens: number
    total_output_tokens: number
    context_window_size: number
    current_usage: number
    used_percentage: number
    remaining_percentage: number
  }

  /** Whether the context exceeds the 200k token threshold */
  exceeds_200k_tokens: boolean

  /** Optional quota utilization info */
  rate_limits?: {
    token?: {
      used_percentage: number
    }
  }

  /** Optional vim mode info */
  vim?: {
    mode: string
  }

  /** Optional agent info */
  agent?: {
    name: string
  }

  /** Optional remote session info */
  remote?: {
    session_id: string
  }

  /** Optional worktree info */
  worktree?: {
    name: string
    path: string
    branch: string
    original_cwd: string
    original_branch: string
  }

  /** Allow additional fields from createBaseHookInput() */
  [key: string]: unknown
}
