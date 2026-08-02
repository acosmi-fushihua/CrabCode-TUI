export const MEMORY_MANAGE_TOOL_NAME = 'MemoryManage'

export const DESCRIPTION = `Interact with the memory system's write/trigger surface (W-MEMORY-SYNERGY W7 + W-MEMORY-KB-UPLIFT 2026-07-17): trigger a dream consolidation, browse / read / draft / promote personal knowledge-base entries, ingest external material (web page / local text file / SQLite read-only snapshot — member features), push knowledge to the user's remote database, list watch targets, or read the self-evolution engine status.

All write-like actions are guarded: dream_now honors the orchestrator gates; every knowledge draft (draft / promote / ingest) is created DISABLED (enabled: false, pending explicit user review in the knowledge entry frontmatter) — this tool can never silently inject content into the user's active memory. Ingested external content additionally passes an SSRF-guarded fetcher, a fail-closed secret scan, and carries provenance frontmatter plus a data-not-instructions banner.`

export const PROMPT = `Manage the memory system (dream trigger / knowledge base / watch inspection / remote sync).

Read-only actions (no confirmation needed):
- action: "knowledge_list" — list personal knowledge-base entries (<config>/knowledge/): path, name, description, enabled, injection mode, pending-review flag. Use before knowledge_read.
- action: "knowledge_read" — read one entry's BODY by absolute "path" (from knowledge_list). Frontmatter is stripped; capped at 512KiB.
- action: "watch_list" — read-only list of 专项检测 (dream-watch) targets. This tool does not create or edit watch targets.
- action: "evolution_status" — read-only self-evolution engine status (fitness snapshot, trial ledger tail, prompt-variant win rates for BOTH families, pending proposals).

Write/trigger actions (each call passes the permission gate):
- action: "dream_now" — run one dream consolidation for the CURRENT project. Respects all safety gates ("lock_held" / "corpus_empty" skips are normal).
- action: "knowledge_draft" — save a DRAFT entry ("title" + "content"; optional "description"/"tags"). Written with enabled: false + pending_review; it participates in recall only after the user explicitly reviews and enables its frontmatter. For durable reference material the user explicitly shared.
- action: "knowledge_promote" — distill existing material into a knowledge draft with provenance: pass "source_path" (a memory/insight .md to carry over) and/or inline "content" (your distillation), plus "title". Use when the user says "把这个存进知识库/收藏这个结论", or to promote a dream insight into durable knowledge. Draft stays disabled pending review.
- action: "knowledge_ingest_url" — MEMBER feature. Fetch an http(s) page ("url") through the SSRF-guarded fetcher into a knowledge draft (html→markdown, provenance source_url, data banner, secret scan fail-closed). This is the "user drops a link → into the knowledge base" path: when the user asks to save a link, call this directly — never hard-route intents.
- action: "knowledge_ingest_file" — MEMBER feature. Import a local .md/.txt/.html file ("file_path", ≤2MB). For office/pdf documents: read them yourself with the vision pipeline, distill in conversation, then knowledge_promote — text extraction for those formats is deliberately out of scope.
- action: "knowledge_ingest_db" — MEMBER feature. Snapshot a read-only query from a LOCAL SQLite file ("db_path" + single SELECT/WITH "sql"). The engine opens read-only, validation is fail-closed (multi-statement / mutating keywords rejected), rows/bytes are capped, and the snapshot lands as a disabled draft.
- action: "knowledge_sync_now" — MEMBER feature. Push enabled knowledge entries + user-global memory to the user-configured remote database (knowledge-sync.json). The enable switch can ONLY be turned on by the user editing that file — if sync is unconfigured/disabled, relay the returned instructions verbatim.

Rules:
- You have NO ability to enable a knowledge entry. Every draft/promote/ingest lands DISABLED and stays inert until the user reviews the returned file and changes its frontmatter to \`enabled: true\`. Never say it takes effect immediately; report the exact path and pending-review state.
- Never use drafts for session-specific or speculative content.
- After any draft-producing action, report the returned knowledge-file path and remind the user that its frontmatter remains disabled and pending review.
- Member-gated actions return an honest upgrade hint for free/unknown-tier accounts — relay it, do not retry.
- dream_now and ingest actions consume model/gateway quota; use them deliberately.
- Returns { ok: false, detail } when a backing service is unavailable — do not retry blindly.`
