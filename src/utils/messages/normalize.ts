/**
 * normalize.ts -- Backward-compatible re-export.
 *
 * This file was a 3409-line monolith. It has been split into:
 *   - normalize-core.ts        (core normalization, merge helpers, stream processing)
 *   - normalize-api.ts         (API normalization pipeline, tool result pairing)
 *   - normalize-attachments.ts (attachment normalization for API)
 *   - normalize-helpers.ts     (filter/strip helper functions)
 *   - normalize-compact.ts     (compact/plan mode support, system reminder wrappers)
 *
 * All exports are re-exported here so that downstream imports remain unchanged.
 */

// ---------------------------------------------------------------------------
// normalize-core.ts -- core normalization, merge helpers, stream processing
// ---------------------------------------------------------------------------
export {
  normalizeMessages,
  normalizeContentFromAPI,
  mergeUserMessagesAndToolResults,
  mergeAssistantMessages,
  mergeUserMessages,
  mergeUserContentBlocks,
  handleMessageFromStream,
} from './normalize-core.js'

// ---------------------------------------------------------------------------
// normalize-api.ts -- API normalization pipeline, tool result pairing
// ---------------------------------------------------------------------------
export {
  reorderAttachmentsForAPI,
  normalizeMessagesForAPI,
  ensureToolResultPairing,
} from './normalize-api.js'

// ---------------------------------------------------------------------------
// normalize-attachments.ts -- attachment normalization for API
// ---------------------------------------------------------------------------
export {
  normalizeAttachmentForAPI,
} from './normalize-attachments.js'

// ---------------------------------------------------------------------------
// normalize-helpers.ts -- filter/strip helper functions
// ---------------------------------------------------------------------------
export {
  stripToolReferenceBlocksFromUserMessage,
  stripCallerFieldFromAssistantMessage,
  filterUnresolvedToolUses,
  filterWhitespaceOnlyAssistantMessages,
  filterOrphanedThinkingOnlyMessages,
  stripSignatureBlocks,
  stripAdvisorBlocks,
  isSystemLocalCommandMessage,
} from './normalize-helpers.js'

// ---------------------------------------------------------------------------
// normalize-compact.ts -- compact/plan mode support
// ---------------------------------------------------------------------------
export {
  wrapInSystemReminder,
  wrapMessagesInSystemReminder,
  neutralizeSystemReminderContent,
  findLastCompactBoundaryIndex,
  getMessagesAfterCompactBoundary,
} from './normalize-compact.js'
