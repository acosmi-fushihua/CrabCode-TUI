/**
 * QuerySource identifies the origin of an API call for analytics/attribution.
 *
 * This is a branded string type — it allows both the known literal values and
 * arbitrary template-literal extensions (e.g. 'repl_main_thread:outputStyle:Explanatory',
 * 'agent:builtin:<agentType>').
 */
export type QuerySource =
  | 'repl_main_thread'
  | 'repl_main_thread:outputStyle:custom'
  | `repl_main_thread:outputStyle:${string}`
  | 'sdk'
  | 'agent:default'
  | 'agent:custom'
  | 'agent:builtin'
  | `agent:builtin:${string}`
  | 'compact'
  | 'hook_agent'
  | 'hook_prompt'
  | 'verification_agent'
  | 'side_question'
  | 'auto_mode'
  | 'bash_classifier'
  | 'permission_explainer'
  | 'session_search'
  | 'model_validation'
  | 'auto_mode_critique'
  | 'auto_dream'
  | 'away_summary'
  | 'web_search_tool'
  | 'web_fetch_apply'
  | 'tool_use_summary_generation'
  | 'memdir_relevance'
  | 'insights'
  | 'rename_generate_name'
  | 'agenticSessionSearch'
  // W-MULTIMODAL-INPUT P2 — chat media sidecar's secondary image-understanding
  // call when the main conversation model is text-only (opt-in degradation of
  // pasted / Read / MCP / PDF-page images to a textual description).
  | 'chat_media_understanding'
  // W-MEMORY-EVOLUTION PR-4 — memory Tier reverse-IPC LLM calls. The
  // orchestrator (Rust data layer) emits `memory/tier/llmCallRequest`; the
  // leader worker's memoryTierProxy runs the SDK call and writes the result
  // back. Background work — deliberately NOT in FOREGROUND_529_RETRY_SOURCES
  // (`withRetry.ts`) so a capacity-cascade 529 fails fast instead of
  // amplifying gateway pressure (no user blocks on a Tier call).
  | 'memory_tier'
  | (string & {})
