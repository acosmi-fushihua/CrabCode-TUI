/**
 * sessionStorage.ts — Barrel re-export file.
 *
 * This module was split into focused sub-modules for maintainability.
 * All original exports are re-exported here so existing `import` paths
 * continue to work without changes.
 *
 * Sub-modules:
 *   sessionStorage-paths.ts      — constants, types, type guards, path helpers
 *   sessionStorage-agent.ts      — agent/subagent transcript paths & metadata
 *   sessionStorage-transcript.ts — transcript loading, chain building, JSONL parsing
 *   sessionStorage-crud.ts       — Project class, record/save functions, session management
 *   sessionStorage-list.ts       — session listing, querying, enrichment
 */

// ─── sessionStorage-paths ───────────────────────────────────────────────────
export {
  // Constants
  MAX_TRANSCRIPT_READ_BYTES,
  // Types
  type TeamInfo,
  // Type guards
  isTranscriptMessage,
  isChainParticipant,
  isEphemeralToolProgress,
  // Path helpers
  getProjectsDir,
  getProjectDir,
  getTranscriptPath,
  getTranscriptPathForSession,
  sessionIdExists,
  getNodeEnv,
  getUserType,
  isCustomTitleEnabled,
} from './sessionStorage-paths.js'

// ─── sessionStorage-agent ───────────────────────────────────────────────────
export {
  // Agent transcript paths
  setAgentTranscriptSubdir,
  clearAgentTranscriptSubdir,
  getAgentTranscriptPath,
  // Agent metadata
  type AgentMetadata,
  writeAgentMetadata,
  readAgentMetadata,
  // Remote agent metadata
  type RemoteAgentMetadata,
  writeRemoteAgentMetadata,
  readRemoteAgentMetadata,
  deleteRemoteAgentMetadata,
  listRemoteAgentMetadata,
  // Agent transcript helpers
  getAgentTranscript,
  extractAgentIdsFromMessages,
  extractTeammateTranscriptsFromTasks,
  loadSubagentTranscripts,
  loadAllSubagentTranscriptsFromDisk,
} from './sessionStorage-agent.js'

// ─── sessionStorage-transcript ──────────────────────────────────────────────
export {
  // Message filtering
  isLoggableMessage,
  cleanMessagesForLogging,
  removeExtraFields,
  // First prompt extraction
  getFirstMeaningfulUserMessageTextContent,
  // Conversation chain building
  buildConversationChain,
  checkResumeConsistency,
  // Transcript file loading
  loadTranscriptFile,
  loadTranscriptFromFile,
} from './sessionStorage-transcript.js'

// ─── sessionStorage-crud ────────────────────────────────────────────────────
export {
  // Project singleton management
  resetProjectFlushStateForTesting,
  resetProjectForTesting,
  setSessionFileForTesting,
  // Record functions
  recordTranscript,
  recordSidechainTranscript,
  recordQueueOperation,
  removeTranscriptMessage,
  recordFileHistorySnapshot,
  recordAttributionSnapshot,
  recordContentReplacement,
  // Session file management
  resetSessionFilePointer,
  adoptResumedSessionFile,
  // Context collapse
  recordContextCollapseCommit,
  recordContextCollapseSnapshot,
  // Flush
  flushSessionStorage,
  // Session metadata save/restore
  saveCustomTitle,
  saveAiGeneratedTitle,
  saveTaskSummary,
  saveTag,
  linkSessionToPR,
  getCurrentSessionTag,
  getCurrentSessionTitle,
  getCurrentSessionAgentColor,
  restoreSessionMetadata,
  clearSessionMetadata,
  reAppendSessionMetadata,
  saveAgentName,
  saveAgentColor,
  saveAgentSetting,
  cacheSessionTitle,
  saveMode,
  saveWorktreeState,
  // Session ID helpers
  getSessionIdFromLog,
  isLiteLog,
  // Session messages cache
  clearSessionMessagesCache,
  doesMessageExistInSession,
} from './sessionStorage-crud.js'

// ─── sessionStorage-list ────────────────────────────────────────────────────
export {
  // Fetching / listing
  fetchLogs,
  loadFullLog,
  searchSessionsByCustomTitle,
  getLastSessionLog,
  loadMessageLogs,
  loadAllProjectsMessageLogs,
  loadAllProjectsMessageLogsProgressive,
  loadSameRepoMessageLogs,
  loadSameRepoMessageLogsProgressive,
  getLogByIndex,
  findUnresolvedToolUse,
  getSessionFilesWithMtime,
  loadAllLogsFromSessionFile,
  getSessionFilesLite,
  enrichLogs,
  type SessionLogResult,
} from './sessionStorage-list.js'
