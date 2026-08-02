/**
 * index.ts -- Re-export barrel for backward compatibility.
 *
 * All 79 files that `import from '../utils/messages.js'` (or similar) will
 * resolve to this file after the original messages.ts is replaced. Every
 * public export from the original monolith is re-exported here so that NO
 * downstream import sites need to change.
 */

// ---------------------------------------------------------------------------
// envelope.ts -- sentinel constants, message predicates, ID derivation, types
// ---------------------------------------------------------------------------
export {
  // Constants
  INTERRUPT_MESSAGE,
  INTERRUPT_MESSAGE_FOR_TOOL_USE,
  CANCEL_MESSAGE,
  REJECT_MESSAGE,
  REJECT_MESSAGE_WITH_REASON_PREFIX,
  SUBAGENT_REJECT_MESSAGE,
  SUBAGENT_REJECT_MESSAGE_WITH_REASON_PREFIX,
  PLAN_REJECTION_PREFIX,
  DENIAL_WORKAROUND_GUIDANCE,
  NO_RESPONSE_REQUESTED,
  SYNTHETIC_TOOL_RESULT_PLACEHOLDER,
  SYNTHETIC_MODEL,
  SYNTHETIC_MESSAGES,

  // Predicate functions
  AUTO_REJECT_MESSAGE,
  DONT_ASK_REJECT_MESSAGE,
  isSyntheticMessage,
  isNotEmptyMessage,
  isToolUseRequestMessage,
  isToolUseResultMessage,
  isCompactBoundaryMessage,
  isThinkingMessage,
  getLastAssistantMessage,
  hasToolCallsInLastAssistantTurn,
  countToolCalls,
  hasSuccessfulToolCall,

  // ID derivation
  deriveUUID,
  deriveShortMessageId,

  // Text extraction
  getAssistantMessageText,
  getUserMessageText,
  extractTextContent,
  getContentText,
  getToolUseID,
  getToolResultIDs,

  // Types
  type StreamingToolUse,
  type StreamingThinking,
} from './envelope.js'

// ---------------------------------------------------------------------------
// factory.ts -- message creation factories
// ---------------------------------------------------------------------------
export {
  withMemoryCorrectionHint,

  // Assistant message factories
  createAssistantMessage,
  createAssistantAPIErrorMessage,

  // User message factories
  createUserMessage,
  prepareUserContent,
  createUserInterruptionMessage,
  createSyntheticUserCaveatMessage,
  formatCommandInputTags,
  createModelSwitchBreadcrumbs,

  // Progress / tool result
  createProgressMessage,
  createToolResultStopMessage,

  // System message factories
  createSystemMessage,
  createPermissionRetryMessage,
  createScheduledTaskFireMessage,
  createStopHookSummaryMessage,
  createTurnDurationMessage,
  createAwaySummaryMessage,
  createMemorySavedMessage,
  createAgentsKilledMessage,
  createApiMetricsMessage,
  createCommandInputMessage,
  createCompactBoundaryMessage,
  createMicrocompactBoundaryMessage,
  createSystemAPIErrorMessage,
  createToolUseSummaryMessage,

  // Command text
  wrapCommandText,
} from './factory.js'

// ---------------------------------------------------------------------------
// normalize.ts -- API normalization pipeline, stream handling
// ---------------------------------------------------------------------------
export {
  // Normalization
  normalizeMessages,
  normalizeMessagesForAPI,
  normalizeContentFromAPI,

  // Merge helpers
  mergeUserMessages,
  mergeAssistantMessages,
  mergeUserMessagesAndToolResults,
  mergeUserContentBlocks,

  // Tool result pairing
  ensureToolResultPairing,

  // Filter / strip
  filterUnresolvedToolUses,
  filterWhitespaceOnlyAssistantMessages,
  filterOrphanedThinkingOnlyMessages,
  stripSignatureBlocks,
  stripAdvisorBlocks,
  stripCallerFieldFromAssistantMessage,
  stripToolReferenceBlocksFromUserMessage,

  // Attachment reordering
  reorderAttachmentsForAPI,

  // Stream handling
  handleMessageFromStream,

  // System reminder wrappers
  wrapInSystemReminder,
  wrapMessagesInSystemReminder,
  neutralizeSystemReminderContent,

  // Compact boundary helpers
  getMessagesAfterCompactBoundary,
  findLastCompactBoundaryIndex,

  // Attachment normalization
  normalizeAttachmentForAPI,

  // System local command check
  isSystemLocalCommandMessage,
} from './normalize.js'

// ---------------------------------------------------------------------------
// render.ts -- TS-only UI helpers
// ---------------------------------------------------------------------------
export {
  extractTag,
  reorderMessagesInUI,

  // Lookups
  buildMessageLookups,
  buildSubagentLookups,
  EMPTY_LOOKUPS,
  EMPTY_STRING_SET,
  type MessageLookups,

  // Lookup accessors
  getSiblingToolUseIDsFromLookup,
  getProgressMessagesFromLookup,
  hasUnresolvedHooksFromLookup,

  // Non-lookup equivalents
  getSiblingToolUseIDs,
  getToolUseIDs,
  hasUnresolvedHooks,

  // UI display helpers
  shouldShowUserMessage,
  isEmptyMessageText,
  stripPromptXMLTags,
  textForResubmit,

  // Classifier denial
  isClassifierDenial,
  buildYoloRejectionMessage,
  buildClassifierUnavailableMessage,

  // Plan phase constant
  PLAN_PHASE4_CONTROL,
} from './render.js'
