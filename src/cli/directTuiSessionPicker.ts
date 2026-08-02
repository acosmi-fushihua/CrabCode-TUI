import type { UUID } from 'node:crypto'

import {
  CRABCODE_TUI_SETUP_PROTOCOL_VERSION,
  CRABCODE_TUI_SETUP_SUBTYPE,
  CrabCodeTuiSessionPickerAckResponseSchema,
  CrabCodeTuiSessionPickerEntrySchema,
  CrabCodeTuiSessionPickerInteractionResponseSchema,
  type CrabCodeTuiSessionPickerEntry,
  type CrabCodeTuiSessionPickerInteractionResponse,
  type CrabCodeTuiSetupRequest,
} from './crabcodeTuiBridgeProtocol.js'
import type { TuiRuntimeOptions } from './tuiRuntimeOptions.js'
import {
  NATIVE_TUI_MAX_FRAME_BYTES,
  nativeTuiSetupControlFrameByteLength,
  type NativeTuiRendererSession,
} from '../entrypoints/nativeTuiRendererSession.js'
import { getOriginalCwd, getSessionId } from '../bootstrap/state.js'
import type { LogOption, SerializedMessage } from '../types/logs.js'
import { checkCrossProjectResume } from '../utils/crossProjectResume.js'
import { errorMessage } from '../utils/errors.js'
import { formatLogMetadata, formatRelativeTimeAgo } from '../utils/format.js'
import { getWorktreePaths } from '../utils/getWorktreePaths.js'
import { getBranch } from '../utils/git.js'
import { getLogDisplayTitle } from '../utils/log.js'
import {
  getSessionIdFromLog,
  saveCustomTitle,
} from '../utils/sessionStorage-crud.js'
import {
  enrichLogs,
  loadAllProjectsMessageLogsProgressive,
  loadFullLog,
  loadSameRepoMessageLogsProgressive,
  searchSessionsByCustomTitle,
  type SessionLogResult,
} from '../utils/sessionStorage-list.js'
import { isCustomTitleEnabled } from '../utils/sessionStorage-paths.js'
import { getFirstMeaningfulUserMessageTextContent } from '../utils/sessionStorage-transcript.js'
import { validateUuid } from '../utils/uuid.js'

type SetupRequester = NativeTuiRendererSession['requestSetup']

export type SessionPickerDependencies = {
  getOriginalCwd(): string
  getCurrentSessionId(): string
  getWorktreePaths(cwd: string): Promise<string[]>
  getBranch(): Promise<string>
  searchSessionsByCustomTitle(
    query: string,
    options: { exact: true },
  ): Promise<LogOption[]>
  loadSameRepoMessageLogsProgressive(
    worktreePaths: string[],
  ): Promise<SessionLogResult>
  loadAllProjectsMessageLogsProgressive(): Promise<SessionLogResult>
  enrichLogs(
    logs: LogOption[],
    startIndex: number,
    count: number,
  ): Promise<{ logs: LogOption[]; nextIndex: number }>
  loadFullLog(log: LogOption): Promise<LogOption>
  saveCustomTitle(
    sessionId: UUID,
    title: string,
    fullPath?: string,
  ): Promise<void>
  isCustomTitleEnabled(): boolean
}

const defaultDependencies: SessionPickerDependencies = {
  getOriginalCwd,
  getCurrentSessionId: getSessionId,
  getWorktreePaths,
  getBranch,
  searchSessionsByCustomTitle,
  loadSameRepoMessageLogsProgressive,
  loadAllProjectsMessageLogsProgressive,
  enrichLogs,
  loadFullLog,
  saveCustomTitle,
  isCustomTitleEnabled,
}

type CatalogUpdate = 'replace' | 'append'

type CatalogState = {
  result: SessionLogResult
  allProjects: boolean
}

const SETUP_BASE = {
  subtype: CRABCODE_TUI_SETUP_SUBTYPE,
  protocol_version: CRABCODE_TUI_SETUP_PROTOCOL_VERSION,
} as const

/**
 * Resolve historical bare/title `--resume` before StructuredIO handoff.
 *
 * Session discovery, metadata mutation, transcript reads, and cross-project
 * policy stay in the existing direct TypeScript authorities. Rust receives
 * only renderer-private presentation values and returns picker actions.
 */
export async function runDirectTuiStartupSessionPicker(
  options: TuiRuntimeOptions,
  requestSetup: SetupRequester,
  dependencies: SessionPickerDependencies = defaultDependencies,
): Promise<void> {
  if (!options.resume || validateUuid(options.resume) !== null) {
    return
  }

  const initialSearch =
    typeof options.resume === 'string' && options.resume.trim()
      ? options.resume.trim()
      : null
  await requestSetup(
    {
      ...SETUP_BASE,
      kind: 'session_picker',
      phase: 'loading',
      initial_search: initialSearch,
    },
    CrabCodeTuiSessionPickerAckResponseSchema,
  )

  if (initialSearch) {
    const matches = await dependencies.searchSessionsByCustomTitle(
      initialSearch,
      { exact: true },
    )
    if (matches.length === 1) {
      await resolveSelectedLog(
        matches[0]!,
        false,
        [],
        options,
        requestSetup,
      )
      return
    }
  }

  const currentCwd = dependencies.getOriginalCwd()
  const [worktreePaths, currentBranch] = await Promise.all([
    dependencies.getWorktreePaths(currentCwd),
    dependencies.getBranch(),
  ])
  const renameEnabled = dependencies.isCustomTitleEnabled()
  const opaqueIds = new Map<string, string>()
  const logsById = new Map<string, LogOption>()
  let nextOpaqueId = 0

  const opaqueIdFor = (log: LogOption): string => {
    const identity = JSON.stringify([
      getSessionIdFromLog(log) ?? null,
      log.leafUuid ?? null,
      log.fullPath ?? null,
      log.projectPath ?? null,
    ])
    const existing = opaqueIds.get(identity)
    if (existing) return existing
    nextOpaqueId += 1
    const id = `session-${nextOpaqueId}`
    opaqueIds.set(identity, id)
    return id
  }

  const visibleLog = (log: LogOption): boolean => {
    const sessionId = getSessionIdFromLog(log)
    if (sessionId && sessionId === dependencies.getCurrentSessionId()) {
      return true
    }
    if (log.customTitle) return true
    if (getFirstMeaningfulUserMessageTextContent(log.messages)) return true
    return Boolean(log.firstPrompt || log.customTitle)
  }

  const entriesFor = (
    logs: LogOption[],
    allProjects: boolean,
  ): CrabCodeTuiSessionPickerEntry[] =>
    logs
      .filter(log => !log.isSidechain)
      .filter(log => !renameEnabled || visibleLog(log))
      .map(log => {
      const id = opaqueIdFor(log)
      logsById.set(id, log)
      const displayTitle = normalizeHistoricalPickerTitle(
        getLogDisplayTitle(log),
      )
      const prInfo = log.prNumber
        ? `pr #${log.prNumber} ${log.prRepository ?? ''}`
        : ''
      const projectPath = log.projectPath ?? currentCwd
      return CrabCodeTuiSessionPickerEntrySchema.parse({
        id,
        title: displayTitle,
        search_text: [
          displayTitle,
          log.gitBranch ?? '',
          log.tag ?? '',
          prInfo,
        ].join(' '),
        metadata:
          formatLogMetadata(log) +
          (allProjects && log.projectPath ? ` · ${log.projectPath}` : ''),
        tag: log.tag ?? null,
        branch: log.gitBranch ?? null,
        group_id: getSessionIdFromLog(log) ?? null,
        in_current_worktree: projectPath === currentCwd,
      })
      })

  const loadCatalog = async (allProjects: boolean): Promise<CatalogState> => ({
    result: allProjects
      ? await dependencies.loadAllProjectsMessageLogsProgressive()
      : await dependencies.loadSameRepoMessageLogsProgressive(worktreePaths),
    allProjects,
  })

  const sendCatalog = async (
    catalog: CatalogState,
    update: CatalogUpdate,
    logs: LogOption[],
  ): Promise<CrabCodeTuiSessionPickerInteractionResponse> => {
    if (update === 'replace') logsById.clear()
    const entries = entriesFor(logs, catalog.allProjects)
    await requestSetup(
      {
        ...SETUP_BASE,
        kind: 'session_picker',
        phase: 'catalog_start',
        update,
        has_more: catalog.result.nextIndex < catalog.result.allStatLogs.length,
        all_projects: catalog.allProjects,
        current_branch: currentBranch || null,
        has_multiple_worktrees: worktreePaths.length > 1,
        rename_enabled: renameEnabled,
      },
      CrabCodeTuiSessionPickerAckResponseSchema,
    )
    await sendChunkedJson(
      entries,
      requestSetup,
    )
    return requestSetup(
      {
        ...SETUP_BASE,
        kind: 'session_picker',
        phase: 'catalog_show',
      },
      CrabCodeTuiSessionPickerInteractionResponseSchema,
    )
  }

  const sendPreview = async (
    id: string,
    log: LogOption,
  ): Promise<CrabCodeTuiSessionPickerInteractionResponse> => {
    await requestSetup(
      {
        ...SETUP_BASE,
        kind: 'session_picker',
        phase: 'preview_start',
        id,
      },
      CrabCodeTuiSessionPickerAckResponseSchema,
    )
    try {
      const fullLog = await dependencies.loadFullLog(log)
      for (const [messageIndex, message] of fullLog.messages.entries()) {
        await sendChunkedMessage(
          id,
          messageIndex,
          message,
          requestSetup,
        )
      }
      return requestSetup(
        {
          ...SETUP_BASE,
          kind: 'session_picker',
          phase: 'preview_complete',
          id,
          metadata: formatRelativeTimeAgo(fullLog.modified),
          message_count: fullLog.messageCount,
          branch: fullLog.gitBranch ?? null,
        },
        CrabCodeTuiSessionPickerInteractionResponseSchema,
      )
    } catch (error) {
      return requestSetup(
        {
          ...SETUP_BASE,
          kind: 'session_picker',
          phase: 'preview_failed',
          id,
          error: singleSafeLine(errorMessage(error)),
        },
        CrabCodeTuiSessionPickerInteractionResponseSchema,
      )
    }
  }

  let catalog = await loadCatalog(false)
  let response = await sendCatalog(catalog, 'replace', catalog.result.logs)
  for (;;) {
    switch (response.decision) {
      case 'select': {
        const log = requireLog(logsById, response.id)
        await resolveSelectedLog(
          log,
          catalog.allProjects,
          worktreePaths,
          options,
          requestSetup,
        )
        return
      }
      case 'preview': {
        const log = requireLog(logsById, response.id)
        response = await sendPreview(response.id, log)
        break
      }
      case 'back':
        response = await requestSetup(
          {
            ...SETUP_BASE,
            kind: 'session_picker',
            phase: 'catalog_show',
          },
          CrabCodeTuiSessionPickerInteractionResponseSchema,
        )
        break
      case 'rename': {
        const log = requireLog(logsById, response.id)
        const sessionId = getSessionIdFromLog(log)
        if (!sessionId) {
          throw new Error(
            'Direct session authority returned a rename row without a session id',
          )
        }
        await dependencies.saveCustomTitle(
          sessionId,
          response.title,
          log.fullPath,
        )
        catalog = await loadCatalog(catalog.allProjects)
        response = await sendCatalog(
          catalog,
          'replace',
          catalog.result.logs,
        )
        break
      }
      case 'reload':
        catalog = await loadCatalog(response.all_projects)
        response = await sendCatalog(
          catalog,
          'replace',
          catalog.result.logs,
        )
        break
      case 'load_more': {
        let page: Awaited<ReturnType<SessionPickerDependencies['enrichLogs']>>
        do {
          page = await dependencies.enrichLogs(
            catalog.result.allStatLogs,
            catalog.result.nextIndex,
            response.count,
          )
          catalog.result.nextIndex = page.nextIndex
        } while (
          page.logs.length === 0 &&
          catalog.result.nextIndex < catalog.result.allStatLogs.length
        )
        response = await sendCatalog(catalog, 'append', page.logs)
        break
      }
      case 'cancel':
        // Rust exits only after this correlated response has been admitted.
        // Keep the direct child alive until the parent closes the setup pipe,
        // matching the historical picker-owned process exit.
        await new Promise<never>(() => {})
    }
  }
}

async function resolveSelectedLog(
  log: LogOption,
  allProjects: boolean,
  worktreePaths: string[],
  options: TuiRuntimeOptions,
  requestSetup: SetupRequester,
): Promise<void> {
  const sessionId = getSessionIdFromLog(log)
  if (!sessionId) {
    throw new Error(
      'Direct session authority returned a resume row without a session id',
    )
  }
  const crossProject = checkCrossProjectResume(
    log,
    allProjects,
    worktreePaths,
  )
  if (
    crossProject.isCrossProject &&
    !crossProject.isSameRepoWorktree
  ) {
    await requestSetup(
      {
        ...SETUP_BASE,
        kind: 'session_picker',
        phase: 'cross_project',
        command: crossProject.command,
      },
      CrabCodeTuiSessionPickerAckResponseSchema,
    )
    return
  }

  // `loadInitialMessages` already accepts a transcript path or UUID. Keeping
  // this established value path preserves same-repo worktree/fork selection
  // without adding a session-switch backend operation.
  options.resume = log.fullPath ?? sessionId
  await requestSetup(
    {
      ...SETUP_BASE,
      kind: 'session_picker',
      phase: 'resolved',
      session_id: sessionId,
    },
    CrabCodeTuiSessionPickerAckResponseSchema,
  )
}

function requireLog(
  logsById: Map<string, LogOption>,
  id: string,
): LogOption {
  const log = logsById.get(id)
  if (!log) {
    throw new Error(
      `Native session picker returned unknown opaque row id ${id}`,
    )
  }
  return log
}

async function sendChunkedMessage(
  id: string,
  messageIndex: number,
  message: SerializedMessage,
  requestSetup: SetupRequester,
): Promise<void> {
  const bytes = Buffer.from(JSON.stringify(message), 'utf8')
  await sendChunkedBytes(
    bytes,
    (chunkIndex, dataBase64, finalChunk) => ({
      ...SETUP_BASE,
      kind: 'session_picker',
      phase: 'preview_message_chunk',
      id,
      message_index: messageIndex,
      chunk_index: chunkIndex,
      data_base64: dataBase64,
      final_chunk: finalChunk,
    }),
    async (chunkIndex, dataBase64, finalChunk) => {
      await requestSetup(
        {
          ...SETUP_BASE,
          kind: 'session_picker',
          phase: 'preview_message_chunk',
          id,
          message_index: messageIndex,
          chunk_index: chunkIndex,
          data_base64: dataBase64,
          final_chunk: finalChunk,
        },
        CrabCodeTuiSessionPickerAckResponseSchema,
      )
    },
  )
}

async function sendChunkedJson(
  value: CrabCodeTuiSessionPickerEntry[],
  requestSetup: SetupRequester,
): Promise<void> {
  await sendChunkedBytes(
    Buffer.from(JSON.stringify(value), 'utf8'),
    (chunkIndex, dataBase64, finalChunk) => ({
      ...SETUP_BASE,
      kind: 'session_picker',
      phase: 'catalog_chunk',
      chunk_index: chunkIndex,
      data_base64: dataBase64,
      final_chunk: finalChunk,
    }),
    async (chunkIndex, dataBase64, finalChunk) => {
      await requestSetup(
        {
          ...SETUP_BASE,
          kind: 'session_picker',
          phase: 'catalog_chunk',
          chunk_index: chunkIndex,
          data_base64: dataBase64,
          final_chunk: finalChunk,
        },
        CrabCodeTuiSessionPickerAckResponseSchema,
      )
    },
  )
}

async function sendChunkedBytes(
  bytes: Buffer,
  requestFactory: (
    chunkIndex: number,
    dataBase64: string,
    finalChunk: boolean,
  ) => CrabCodeTuiSetupRequest,
  sendChunk: (
    chunkIndex: number,
    dataBase64: string,
    finalChunk: boolean,
  ) => Promise<void>,
): Promise<void> {
  if (bytes.length > NATIVE_TUI_MAX_FRAME_BYTES) {
    throw new Error(
      `Native TUI logical transfer has ${bytes.length} bytes; transport limit is ${NATIVE_TUI_MAX_FRAME_BYTES}`,
    )
  }
  let offset = 0
  let chunkIndex = 0
  while (offset < bytes.length) {
    // Base64 contains no JSON-escaped characters. Measuring the full empty
    // envelope therefore gives the exact character budget for this index.
    // Use `false`, the longer boolean spelling, so a final `true` frame can
    // only be one byte smaller.
    const emptyRequest = requestFactory(chunkIndex, '', false)
    const emptyBytes = nativeTuiSetupControlFrameByteLength(emptyRequest)
    const base64Capacity = NATIVE_TUI_MAX_FRAME_BYTES - emptyBytes
    const rawCapacity = Math.floor(base64Capacity / 4) * 3
    if (rawCapacity <= 0) {
      throw new Error(
        'Native TUI setup envelope leaves no room for a transfer payload',
      )
    }
    const end = Math.min(bytes.length, offset + rawCapacity)
    const dataBase64 = bytes.subarray(offset, end).toString('base64')
    const finalChunk = end === bytes.length
    const measured = nativeTuiSetupControlFrameByteLength(
      requestFactory(chunkIndex, dataBase64, finalChunk),
    )
    if (measured > NATIVE_TUI_MAX_FRAME_BYTES) {
      throw new Error(
        `Native TUI chunk frame has ${measured} bytes; transport limit is ${NATIVE_TUI_MAX_FRAME_BYTES}`,
      )
    }
    await sendChunk(chunkIndex, dataBase64, finalChunk)
    offset = end
    chunkIndex += 1
  }
}

function normalizeHistoricalPickerTitle(value: string): string {
  return value.replace(/\s+/g, ' ').trim()
}

function singleSafeLine(value: string): string {
  return value.replace(/[\u0000-\u001f\u007f]+/g, ' ').slice(0, 1024)
}
