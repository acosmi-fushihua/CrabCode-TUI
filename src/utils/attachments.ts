/**
 * attachments.ts -- Backward-compatible re-export barrel.
 *
 * This file was a 3997-line monolith. It has been split into:
 *   - attachments-types.ts        (type/interface definitions, config constants)
 *   - attachments-memory.ts       (memory surfacing, prefetch, deduplication)
 *   - attachments-delta.ts        (deferred tools, agent listing, MCP instructions, date change deltas)
 *   - attachments-agent.ts        (agent/skill listing, queued commands, skill name tracking)
 *   - attachments-parse.ts        (@ mention extraction, MCP resource parsing, tool tracking)
 *   - attachments-orchestrator.ts (getAttachments, getAttachmentMessages, createAttachmentMessage,
 *                                  file generation, plan/auto mode, IDE, diagnostics, teammate mailbox)
 *
 * All exports are re-exported here so that downstream imports remain unchanged.
 */

// ---------------------------------------------------------------------------
// attachments-types.ts -- type definitions, config constants
// ---------------------------------------------------------------------------
export type {
  FileAttachment,
  CompactFileReferenceAttachment,
  PDFReferenceAttachment,
  AlreadyReadFileAttachment,
  AgentMentionAttachment,
  AsyncHookResponseAttachment,
  HookAttachment,
  HookPermissionDecisionAttachment,
  HookSystemMessageAttachment,
  HookCancelledAttachment,
  HookErrorDuringExecutionAttachment,
  HookSuccessAttachment,
  HookNonBlockingErrorAttachment,
  Attachment,
  TeammateMailboxAttachment,
  TeamContextAttachment,
  MemoryPrefetch,
} from './attachments-types.js'

export {
  TODO_REMINDER_CONFIG,
  PLAN_MODE_ATTACHMENT_CONFIG,
  AUTO_MODE_ATTACHMENT_CONFIG,
  RELEVANT_MEMORIES_CONFIG,
  VERIFY_PLAN_REMINDER_CONFIG,
} from './attachments-types.js'

// ---------------------------------------------------------------------------
// attachments-memory.ts -- memory surfacing, prefetch, deduplication
// ---------------------------------------------------------------------------
export {
  memoryFilesToAttachments,
  collectSurfacedMemories,
  memoryHeader,
  startRelevantMemoryPrefetch,
  readMemoriesForSurfacing,
  filterDuplicateMemoryAttachments,
  getDirectoriesToProcess,
} from './attachments-memory.js'

// ---------------------------------------------------------------------------
// attachments-delta.ts -- deferred tools, agent listing, MCP instructions
// ---------------------------------------------------------------------------
export {
  getDateChangeAttachments,
  getDeferredToolsDeltaAttachment,
  getAgentListingDeltaAttachment,
  getMcpInstructionsDeltaAttachment,
  getUltrathinkEffortAttachment,
} from './attachments-delta.js'

// ---------------------------------------------------------------------------
// attachments-agent.ts -- agent/skill listing, queued commands
// ---------------------------------------------------------------------------
export {
  getQueuedCommandAttachments,
  getAgentPendingMessageAttachments,
  resetSentSkillNames,
  suppressNextSkillListing,
  filterToBundledAndMcp,
} from './attachments-agent.js'

// ---------------------------------------------------------------------------
// attachments-parse.ts -- @-mention extraction, tool tracking
// ---------------------------------------------------------------------------
export {
  extractAtMentionedFiles,
  extractMcpResourceMentions,
  extractAgentMentions,
  parseAtMentionedFileLines,
  collectRecentSuccessfulTools,
} from './attachments-parse.js'

// ---------------------------------------------------------------------------
// attachments-orchestrator.ts -- main orchestrator + helpers
// ---------------------------------------------------------------------------
export {
  getAttachments,
  getAttachmentMessages,
  createAttachmentMessage,
  getChangedFiles,
  tryGetPDFReference,
  generateFileAttachment,
  getVerifyPlanReminderTurnCount,
  getCompactionReminderAttachment,
  getContextEfficiencyAttachment,
} from './attachments-orchestrator.js'
