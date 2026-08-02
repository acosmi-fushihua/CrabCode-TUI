// Phase -1 stub: 待后续实现
// Type definitions for the file suggestion hook system.
// Used by src/hooks/fileSuggestions.ts and src/utils/hooks.ts.
//
// FileSuggestionCommandInput extends the base hook input with a query string
// that represents the user's partial file path input for typeahead.

/**
 * Input payload for the fileSuggestion hook command.
 * Extends the base hook input (session_id, transcript_path, cwd, etc.)
 * with a query field containing the user's partial path.
 */
export type FileSuggestionCommandInput = {
  /** Current session ID */
  session_id: string
  /** Path to the session transcript */
  transcript_path: string
  /** Current working directory */
  cwd: string
  /** Permission mode, if set */
  permission_mode?: string
  /** The partial file path the user has typed so far */
  query: string
}
