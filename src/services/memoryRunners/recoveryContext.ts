import { lstat, realpath } from 'fs/promises'
import {
  dirname,
  isAbsolute,
  join,
  normalize,
  parse,
  resolve,
} from 'path'
import {
  runWithSessionOverride,
} from '../../bootstrap/state.js'
import { getAutoMemPath } from '../../memdir/paths.js'
import { getDefaultAppState } from '../../state/AppStateStore.js'
import { getEmptyToolPermissionContext } from '../../Tool.js'
import { getTools } from '../../tools.js'
import { BASH_TOOL_NAME } from '../../tools/BashTool/toolName.js'
import { FILE_EDIT_TOOL_NAME } from '../../tools/FileEditTool/constants.js'
import { FILE_READ_TOOL_NAME } from '../../tools/FileReadTool/prompt.js'
import { FILE_WRITE_TOOL_NAME } from '../../tools/FileWriteTool/prompt.js'
import { GLOB_TOOL_NAME } from '../../tools/GlobTool/prompt.js'
import { GREP_TOOL_NAME } from '../../tools/GrepTool/prompt.js'
import { REPL_TOOL_NAME } from '../../tools/REPLTool/constants.js'
import { asSessionId } from '../../types/ids.js'
import {
  loadMessagesFromJsonlPathAtLeaf,
} from '../../utils/conversationRecovery.js'
import { runWithCwdOverride } from '../../utils/cwd.js'
import {
  createFileStateCacheWithSizeLimit,
} from '../../utils/fileStateCache.js'
import { buildSideQuestionFallbackParams } from '../../utils/queryContext.js'
import {
  getProjectDir,
  MAX_TRANSCRIPT_READ_BYTES,
} from '../../utils/sessionStorage.js'
import { validateUuid } from '../../utils/uuid.js'
import {
  MEMORY_RECOVERY_SCHEMA_VERSION,
  type MemoryRecoveryLocator,
} from './recoveryProtocol.js'
import type { MemoryRunnerStopHookContext } from './turnEnd.js'

export {
  MEMORY_RECOVERY_SCHEMA_VERSION,
  type MemoryRecoveryLocator,
} from './recoveryProtocol.js'

export type MemoryRecoveryFailureDisposition = 'retryable' | 'dead_letter'

export class MemoryRecoveryContextError extends Error {
  constructor(
    readonly code: string,
    readonly disposition: MemoryRecoveryFailureDisposition,
    message: string,
    options?: { cause?: unknown },
  ) {
    super(message, options)
    this.name = 'MemoryRecoveryContextError'
  }
}

const RECOVERY_TOOL_NAMES = new Set([
  BASH_TOOL_NAME,
  FILE_EDIT_TOOL_NAME,
  FILE_READ_TOOL_NAME,
  FILE_WRITE_TOOL_NAME,
  GLOB_TOOL_NAME,
  GREP_TOOL_NAME,
  REPL_TOOL_NAME,
])

function permanent(code: string, message: string): never {
  throw new MemoryRecoveryContextError(code, 'dead_letter', message)
}

function transient(code: string, message: string, cause?: unknown): never {
  throw new MemoryRecoveryContextError(code, 'retryable', message, { cause })
}

function validateAbsolutePath(field: string, value: string): void {
  if (
    value.length === 0 ||
    value.includes('\0') ||
    !isAbsolute(value) ||
    normalize(value) === parse(value).root ||
    (process.platform === 'win32' && value.startsWith('\\\\'))
  ) {
    permanent('invalid_recovery_path', `${field} is not a safe absolute path`)
  }
}

function comparablePath(value: string): string {
  const normalized = normalize(resolve(value))
  return process.platform === 'win32' ? normalized.toLowerCase() : normalized
}

function pathsEqual(left: string, right: string): boolean {
  return comparablePath(left) === comparablePath(right)
}

async function requirePrivateRegularTranscript(path: string): Promise<void> {
  let metadata
  try {
    metadata = await lstat(path)
  } catch (error) {
    transient(
      'transcript_unavailable',
      'the authoritative transcript is not currently readable',
      error,
    )
  }
  if (metadata.isSymbolicLink() || !metadata.isFile()) {
    permanent(
      'unsafe_transcript_type',
      'the authoritative transcript must be a regular non-symlink file',
    )
  }
  if (metadata.size > MAX_TRANSCRIPT_READ_BYTES) {
    permanent(
      'transcript_too_large',
      `the authoritative transcript exceeds ${MAX_TRANSCRIPT_READ_BYTES} bytes`,
    )
  }
  if (
    process.platform !== 'win32' &&
    typeof process.getuid === 'function' &&
    metadata.uid !== process.getuid()
  ) {
    permanent(
      'transcript_owner_mismatch',
      'the authoritative transcript is not owned by the current user',
    )
  }
  if (process.platform !== 'win32' && (metadata.mode & 0o022) !== 0) {
    permanent(
      'transcript_permissions_unsafe',
      'the authoritative transcript is group/world writable',
    )
  }
}

async function requirePrivateDirectory(
  field: string,
  path: string,
): Promise<string> {
  let metadata
  try {
    metadata = await lstat(path)
  } catch (error) {
    transient(
      'recovery_directory_unavailable',
      `${field} is not currently readable`,
      error,
    )
  }
  if (metadata.isSymbolicLink() || !metadata.isDirectory()) {
    permanent(
      'unsafe_recovery_directory',
      `${field} must be a non-symlink directory`,
    )
  }
  if (
    process.platform !== 'win32' &&
    typeof process.getuid === 'function' &&
    metadata.uid !== process.getuid()
  ) {
    permanent(
      'recovery_directory_owner_mismatch',
      `${field} is not owned by the current user`,
    )
  }
  if (process.platform !== 'win32' && (metadata.mode & 0o022) !== 0) {
    permanent(
      'recovery_directory_permissions_unsafe',
      `${field} is group/world writable`,
    )
  }
  try {
    return await realpath(path)
  } catch (error) {
    transient(
      'recovery_directory_realpath_failed',
      `${field} could not be canonicalized`,
      error,
    )
  }
}

function validateLocatorShape(
  locator: MemoryRecoveryLocator,
  expectedTriggerId: string,
  expectedKind: 'dream' | 'extract',
): void {
  if (locator.recovery_schema_version !== MEMORY_RECOVERY_SCHEMA_VERSION) {
    permanent(
      'unsupported_recovery_schema',
      'the durable recovery locator schema is unsupported',
    )
  }
  if (
    locator.trigger_id !== expectedTriggerId ||
    locator.kind !== expectedKind
  ) {
    permanent(
      'recovery_subject_mismatch',
      'the recovery locator does not match the claimed trigger',
    )
  }
  if (
    validateUuid(locator.session_id) === null ||
    validateUuid(locator.current_session_id) === null ||
    validateUuid(locator.context_leaf_uuid) === null
  ) {
    permanent(
      'invalid_recovery_uuid',
      'the recovery locator contains a non-UUID session or context leaf',
    )
  }
  for (const [field, value] of [
    ['project_cwd', locator.project_cwd],
    ['transcript_path', locator.transcript_path],
    ['project_state_dir', locator.project_state_dir],
    ['memory_dir', locator.memory_dir],
  ] as const) {
    validateAbsolutePath(field, value)
  }
}

/**
 * Rebuild a Memory runner context from current trusted runtime state while
 * binding every async descendant to the journaled session and cwd.
 *
 * Only the transcript locator is durable. Tools, settings, model, credentials,
 * AppState and callbacks are rebuilt from the live process; CacheSafeParams is
 * never serialized. This is a fresh background fork and does not claim
 * byte-identical prompt-cache equivalence with the original foreground turn.
 */
export async function withAuthoritativeMemoryRecoveryContext<T>(
  locator: MemoryRecoveryLocator,
  expectedTriggerId: string,
  expectedKind: 'dream' | 'extract',
  signal: AbortSignal,
  run: (context: MemoryRunnerStopHookContext) => Promise<T>,
): Promise<T> {
  validateLocatorShape(locator, expectedTriggerId, expectedKind)

  return runWithSessionOverride(
    {
      sessionId: asSessionId(locator.session_id),
      projectRootCwd: locator.project_cwd,
      sessionProjectDir: dirname(locator.transcript_path),
    },
    () =>
      runWithCwdOverride(locator.project_cwd, async () => {
        const expectedTranscript = join(
          getProjectDir(locator.project_cwd),
          `${locator.session_id}.jsonl`,
        )
        if (!pathsEqual(locator.transcript_path, expectedTranscript)) {
          permanent(
            'transcript_locator_mismatch',
            'the transcript path does not match the current session/project authority',
          )
        }
        if (!pathsEqual(dirname(locator.memory_dir), locator.project_state_dir)) {
          permanent(
            'memory_state_locator_mismatch',
            'memory_dir and project_state_dir do not form the persisted project pair',
          )
        }
        if (!pathsEqual(getAutoMemPath(), locator.memory_dir)) {
          permanent(
            'memory_authority_changed',
            'the current authoritative memory root no longer matches the durable work item',
          )
        }

        const [
          canonicalProjectDir,
          canonicalMemoryStateDir,
          canonicalMemoryDir,
        ] = await Promise.all([
          requirePrivateDirectory(
            'transcript project directory',
            dirname(locator.transcript_path),
          ),
          requirePrivateDirectory(
            'memory project state directory',
            locator.project_state_dir,
          ),
          requirePrivateDirectory('memory directory', locator.memory_dir),
        ])
        let canonicalTranscript
        try {
          canonicalTranscript = await realpath(locator.transcript_path)
        } catch (error) {
          transient(
            'transcript_realpath_failed',
            'the authoritative transcript could not be canonicalized',
            error,
          )
        }
        if (
          !pathsEqual(dirname(canonicalTranscript), canonicalProjectDir) ||
          !pathsEqual(canonicalMemoryStateDir, dirname(canonicalMemoryDir))
        ) {
          permanent(
            'canonical_recovery_path_mismatch',
            'canonical recovery paths escaped their persisted authority roots',
          )
        }
        await requirePrivateRegularTranscript(locator.transcript_path)

        const loaded = await loadMessagesFromJsonlPathAtLeaf(
          locator.transcript_path,
          locator.context_leaf_uuid,
        )
        if (
          loaded.sessionId !== locator.session_id ||
          loaded.cwd === undefined ||
          !pathsEqual(loaded.cwd, locator.project_cwd) ||
          loaded.messages.some(
            message => message.sessionId !== locator.session_id,
          )
        ) {
          permanent(
            'transcript_identity_mismatch',
            'the exact transcript chain does not match the durable session/cwd authority',
          )
        }

        let appState = getDefaultAppState()
        const permissionContext = {
          ...getEmptyToolPermissionContext(),
          shouldAvoidPermissionPrompts: true,
        }
        appState = {
          ...appState,
          toolPermissionContext: permissionContext,
          mcp: {
            ...appState.mcp,
            clients: [],
            tools: [],
            commands: [],
            resources: {},
          },
        }
        const getAppState = () => appState
        const setAppState = (
          update: (previous: typeof appState) => typeof appState,
        ): void => {
          appState = update(appState)
        }
        const tools = getTools(permissionContext).filter(tool =>
          RECOVERY_TOOL_NAMES.has(tool.name),
        )
        if (
          !tools.some(tool => tool.name === FILE_READ_TOOL_NAME) ||
          !tools.some(
            tool =>
              tool.name === FILE_EDIT_TOOL_NAME ||
              tool.name === FILE_WRITE_TOOL_NAME,
          )
        ) {
          transient(
            'recovery_toolset_unavailable',
            'the current runtime cannot assemble the required Memory tool subset',
          )
        }

        const cacheSafeParams = await buildSideQuestionFallbackParams({
          tools,
          commands: [],
          mcpClients: [],
          messages: loaded.messages,
          readFileState: createFileStateCacheWithSizeLimit(
            100,
            10_000_000,
          ),
          getAppState,
          setAppState,
          customSystemPrompt: undefined,
          appendSystemPrompt: undefined,
          thinkingConfig: undefined,
          agents: [],
        })
        cacheSafeParams.toolUseContext.setAppStateForTasks = setAppState
        const abort = (): void => {
          if (!cacheSafeParams.toolUseContext.abortController.signal.aborted) {
            cacheSafeParams.toolUseContext.abortController.abort(signal.reason)
          }
        }
        if (signal.aborted) abort()
        else signal.addEventListener('abort', abort, { once: true })

        try {
          return await run({
            messages: loaded.messages,
            systemPrompt: cacheSafeParams.systemPrompt,
            userContext: cacheSafeParams.userContext,
            systemContext: cacheSafeParams.systemContext,
            toolUseContext: cacheSafeParams.toolUseContext,
            querySource: 'memory_recovery',
          })
        } finally {
          signal.removeEventListener('abort', abort)
        }
      }),
  )
}
