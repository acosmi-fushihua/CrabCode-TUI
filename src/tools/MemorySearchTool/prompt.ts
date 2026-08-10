export const MEMORY_SEARCH_TOOL_NAME = 'MemorySearch'

/**
 * Kept next to explicit MemorySearch results so a historical environment or
 * capability observation cannot silently override the current executable.
 */
export const MEMORY_SEARCH_DRIFT_REMINDER =
  '<system-reminder>Memory search results are historical snapshots, not live capability or environment state. Verify current tool availability, network access, configuration, and code behavior against the current runtime before reporting them as facts. If a live observation conflicts, trust the live observation and treat the memory as stale.</system-reminder>'

export const DESCRIPTION = `Search your long-term memory, the user's personal knowledge base, and evolution artifacts for context relevant to the current task.

Searchable content spans three scopes (W-MEMORY-LIFECYCLE K4/K9):
- Project memory: short markdown notes you (CrabCode) saved for this project across sessions — user preferences, project facts, feedback, references, plus evolution artifacts (dream insights and evolution reports).
- Global memory: user-level memories that apply in every session and project.
- Personal knowledge base: markdown entries the user curated themselves.

The search is keyword-based (multilingual lexical search) and returns the most relevant entries with their scope. Use it when the user references prior decisions, conventions, or context that may have been recorded but is not already in view. Results are read-only — to view a full note, open its path with the file Read tool.`

export const PROMPT = `Search long-term memory, the personal knowledge base, and evolution artifacts for relevant context.

Parameters:
- query (required): what to look for. Keywords work best (e.g. "deploy mirror",
  "测试入口", a tool name, a project convention). Multilingual.
- top_k (optional): max results to return (default 5).

Returns the most relevant entries with their file path, name, relevance score,
a snippet, and the scope they came from (project memory / global memory /
knowledge base). Read-only: it does not modify memory. To read a full note,
use the file Read tool on the returned path.

When to use:
- The user refers to a past decision, preference, or convention not in context.
- You need project-specific facts that may have been saved previously.
- The user's personal knowledge base may hold curated reference material.

The search is lexical (keyword/word-overlap), so prefer distinctive terms over
full sentences. Returns nothing when memory is unavailable or empty.`
