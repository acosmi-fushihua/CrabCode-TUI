import { z } from 'zod/v4'

import type { AppState } from '../state/AppStateStore.js'
import type { ToolUseContext } from '../Tool.js'
import type {
  LocalJSXCommandContext,
  LocalJSXCommandOnDone,
} from '../types/command.js'
import type { Message } from '../types/message.js'
import { logForDebugging } from '../utils/debug.js'

/**
 * Closed process-private value actions for retained fixed-history commands.
 *
 * These actions call the existing command owners. They do not add a public
 * control method, a remote route, or a second implementation of the
 * persistence transactions. Renderer-only state is supplied explicitly by
 * the direct TUI surface and never installed for the ordinary SDK route.
 */

export const DIRECT_TUI_RETAINED_COMMAND_ACTION_KINDS = [
  'retained.identity.snapshot',
  'retained.color.apply',
  'retained.rename.apply',
  'retained.vim.toggle',
  'retained.brief.toggle',
] as const

const RetainedCommandActionKindSchema = z.enum(
  DIRECT_TUI_RETAINED_COMMAND_ACTION_KINDS,
)

export const DirectTuiRetainedCommandActionSchema = z.discriminatedUnion(
  'kind',
  [
    z.object({ kind: z.literal('retained.identity.snapshot') }).strict(),
    z
      .object({
        kind: z.literal('retained.color.apply'),
        argument: z.string(),
      })
      .strict(),
    z
      .object({
        kind: z.literal('retained.rename.apply'),
        argument: z.string(),
      })
      .strict(),
    z.object({ kind: z.literal('retained.vim.toggle') }).strict(),
    z.object({ kind: z.literal('retained.brief.toggle') }).strict(),
  ],
)

export type DirectTuiRetainedCommandAction = z.infer<
  typeof DirectTuiRetainedCommandActionSchema
>

const AgentColorSchema = z.enum([
  'red',
  'blue',
  'green',
  'yellow',
  'purple',
  'orange',
  'pink',
  'cyan',
])

const RetainedCommandErrorCodeSchema = z.enum([
  'argument_required',
  'invalid_argument',
  'teammate_restricted',
  'name_generation_unavailable',
  'command_unavailable',
  'not_entitled',
  'surface_unavailable',
  'authority_failure',
])

const DirectTuiRetainedColorResultSchema = z
  .object({
    kind: z.literal('retained.color.updated'),
    color: AgentColorSchema.nullable(),
  })
  .strict()

const DirectTuiRetainedIdentityResultSchema = z
  .object({
    kind: z.literal('retained.identity.snapshot'),
    name: z.string().nullable(),
    color: AgentColorSchema.nullable(),
  })
  .strict()

const DirectTuiRetainedRenameResultSchema = z
  .object({
    kind: z.literal('retained.rename.updated'),
    name: z.string().min(1),
  })
  .strict()

const DirectTuiRetainedVimResultSchema = z
  .object({
    kind: z.literal('retained.vim.updated'),
    editor_mode: z.enum(['normal', 'vim']),
  })
  .strict()

const DirectTuiRetainedBriefResultSchema = z
  .object({
    kind: z.literal('retained.brief.updated'),
    enabled: z.boolean(),
    reminder_injected: z.boolean(),
  })
  .strict()

const DirectTuiRetainedCommandErrorSchema = z
  .object({
    kind: z.literal('retained_command_error'),
    action_kind: RetainedCommandActionKindSchema,
    code: RetainedCommandErrorCodeSchema,
  })
  .strict()

export const DirectTuiRetainedCommandResultSchema = z.discriminatedUnion(
  'kind',
  [
    DirectTuiRetainedIdentityResultSchema,
    DirectTuiRetainedColorResultSchema,
    DirectTuiRetainedRenameResultSchema,
    DirectTuiRetainedVimResultSchema,
    DirectTuiRetainedBriefResultSchema,
    DirectTuiRetainedCommandErrorSchema,
  ],
)

export type DirectTuiRetainedCommandResult = z.infer<
  typeof DirectTuiRetainedCommandResultSchema
>

export type DirectTuiRetainedCommandSurface = {
  getAppState: () => AppState
  setAppState: (updater: (previous: AppState) => AppState) => void
  getMessages: () => readonly Message[]
  appendMetaMessages: (contents: readonly string[]) => void
}

type CommandInvocation = {
  completionCalled: boolean
  stateUpdateObserved: boolean
  metaMessages: readonly string[]
}

type VimInvocation = {
  editorMode: 'normal' | 'vim'
}

export type DirectTuiRetainedCommandDependencies = {
  isTeammate: () => boolean | Promise<boolean>
  isBriefCommandEnabled: () => boolean | Promise<boolean>
  isBriefEntitled: () => boolean | Promise<boolean>
  invokeColor: (
    surface: DirectTuiRetainedCommandSurface,
    argument: string,
  ) => Promise<CommandInvocation>
  invokeRename: (
    surface: DirectTuiRetainedCommandSurface,
    argument: string,
  ) => Promise<CommandInvocation>
  invokeVim: () => Promise<VimInvocation>
  invokeBrief: (
    surface: DirectTuiRetainedCommandSurface,
  ) => Promise<CommandInvocation>
  reportError: (error: unknown) => void | Promise<void>
}

const COLOR_VALUES = new Set(AgentColorSchema.options)
const COLOR_RESET_ALIASES = new Set([
  'default',
  'reset',
  'none',
  'gray',
  'grey',
])

function retainedCommandError(
  actionKind: DirectTuiRetainedCommandAction['kind'],
  code: z.infer<typeof RetainedCommandErrorCodeSchema>,
): DirectTuiRetainedCommandResult {
  return {
    kind: 'retained_command_error',
    action_kind: actionKind,
    code,
  }
}

async function reportErrorSafely(
  dependencies: DirectTuiRetainedCommandDependencies,
  error: unknown,
): Promise<void> {
  try {
    await dependencies.reportError(error)
  } catch {
    // A diagnostic sink cannot promote a request-local failure to a process
    // failure.
  }
}

export async function handleDirectTuiRetainedCommandAction(
  action: DirectTuiRetainedCommandAction,
  surface: DirectTuiRetainedCommandSurface | undefined,
  dependencies: DirectTuiRetainedCommandDependencies =
    createDefaultDirectTuiRetainedCommandDependencies(),
): Promise<DirectTuiRetainedCommandResult> {
  if (action.kind !== 'retained.vim.toggle' && surface === undefined) {
    return retainedCommandError(action.kind, 'surface_unavailable')
  }

  try {
    let result: DirectTuiRetainedCommandResult

    switch (action.kind) {
      case 'retained.identity.snapshot': {
        const identity = surface!.getAppState().standaloneAgentContext
        result = {
          kind: 'retained.identity.snapshot',
          name: identity?.name || null,
          color: identity?.color ?? null,
        }
        break
      }

      case 'retained.color.apply': {
        const colorArgument = action.argument.trim().toLowerCase()
        if (colorArgument.length === 0) {
          result = retainedCommandError(action.kind, 'argument_required')
          break
        }
        if (
          !COLOR_VALUES.has(
            colorArgument as z.infer<typeof AgentColorSchema>,
          ) &&
          !COLOR_RESET_ALIASES.has(colorArgument)
        ) {
          result = retainedCommandError(action.kind, 'invalid_argument')
          break
        }
        if (await dependencies.isTeammate()) {
          result = retainedCommandError(action.kind, 'teammate_restricted')
          break
        }

        const invocation = await dependencies.invokeColor(
          surface!,
          action.argument,
        )
        if (!invocation.completionCalled || !invocation.stateUpdateObserved) {
          result = retainedCommandError(action.kind, 'authority_failure')
          break
        }

        const color = surface!.getAppState().standaloneAgentContext?.color
        const expectedColor = COLOR_RESET_ALIASES.has(colorArgument)
          ? null
          : AgentColorSchema.parse(colorArgument)
        if ((color ?? null) !== expectedColor) {
          result = retainedCommandError(action.kind, 'authority_failure')
          break
        }
        result = { kind: 'retained.color.updated', color: color ?? null }
        break
      }

      case 'retained.rename.apply': {
        if (await dependencies.isTeammate()) {
          result = retainedCommandError(action.kind, 'teammate_restricted')
          break
        }

        const invocation = await dependencies.invokeRename(
          surface!,
          action.argument,
        )
        if (!invocation.completionCalled || !invocation.stateUpdateObserved) {
          result = retainedCommandError(
            action.kind,
            action.argument.trim().length === 0
              ? 'name_generation_unavailable'
              : 'authority_failure',
          )
          break
        }

        const name = surface!.getAppState().standaloneAgentContext?.name
        if (
          !name ||
          (action.argument.trim().length > 0 &&
            name !== action.argument.trim())
        ) {
          result = retainedCommandError(action.kind, 'authority_failure')
          break
        }
        result = { kind: 'retained.rename.updated', name }
        break
      }

      case 'retained.vim.toggle': {
        const invocation = await dependencies.invokeVim()
        result = {
          kind: 'retained.vim.updated',
          editor_mode: invocation.editorMode,
        }
        break
      }

      case 'retained.brief.toggle': {
        const current = surface!.getAppState().isBriefOnly
        // The fixed owner always permits the off transition. Re-checking the
        // catalog gate is only valid while turning the feature on.
        if (!current && !(await dependencies.isBriefCommandEnabled())) {
          result = retainedCommandError(action.kind, 'command_unavailable')
          break
        }
        if (!current && !(await dependencies.isBriefEntitled())) {
          result = retainedCommandError(action.kind, 'not_entitled')
          break
        }

        const invocation = await dependencies.invokeBrief(surface!)
        if (!invocation.completionCalled || !invocation.stateUpdateObserved) {
          result = retainedCommandError(action.kind, 'authority_failure')
          break
        }
        const enabled = surface!.getAppState().isBriefOnly
        if (enabled === current) {
          result = retainedCommandError(action.kind, 'authority_failure')
          break
        }
        if (invocation.metaMessages.length > 0) {
          surface!.appendMetaMessages(invocation.metaMessages)
        }
        result = {
          kind: 'retained.brief.updated',
          enabled,
          reminder_injected: invocation.metaMessages.length > 0,
        }
        break
      }
    }

    return DirectTuiRetainedCommandResultSchema.parse(result)
  } catch (error) {
    await reportErrorSafely(dependencies, error)
    return retainedCommandError(action.kind, 'authority_failure')
  }
}

export function createDefaultDirectTuiRetainedCommandDependencies(): DirectTuiRetainedCommandDependencies {
  return {
    async isTeammate() {
      const { isTeammate } = await import('../utils/teammate.js')
      return isTeammate()
    },
    async isBriefCommandEnabled() {
      const { default: briefCommand } = await import('../commands/brief.js')
      return briefCommand.isEnabled()
    },
    async isBriefEntitled() {
      const { isBriefEntitled } = await import(
        '../tools/BriefTool/BriefTool.js'
      )
      return isBriefEntitled()
    },
    async invokeColor(surface, argument) {
      const { call } = await import('../commands/color/color.js')
      return captureLocalJsxInvocation(surface, (onDone, context) =>
        call(onDone, context, argument),
      )
    },
    async invokeRename(surface, argument) {
      const { call } = await import('../commands/rename/rename.js')
      return captureLocalJsxInvocation(surface, (onDone, context) =>
        call(onDone, context, argument),
      )
    },
    async invokeVim() {
      const [{ call }, { getGlobalConfig }] = await Promise.all([
        import('../commands/vim/vim.js'),
        import('../utils/config.js'),
      ])
      // The fixed/current vim owner does not read its context. Supplying the
      // minimum structural value here is deliberate and source-audited.
      await call('', {} as LocalJSXCommandContext)
      const editorMode = getGlobalConfig().editorMode
      if (editorMode !== 'normal' && editorMode !== 'vim') {
        throw new Error('vim owner did not persist a supported editor mode')
      }
      return { editorMode }
    },
    async invokeBrief(surface) {
      const { default: briefCommand } = await import('../commands/brief.js')
      const module = await briefCommand.load()
      return captureLocalJsxInvocation(surface, (onDone, context) =>
        module.call(onDone, context),
      )
    },
    reportError(error) {
      logForDebugging(
        `[direct-tui-retained-command] authority failed: ${
          error instanceof Error ? error.name : 'unknown'
        }`,
        { level: 'warn' },
      )
    },
  }
}

async function captureLocalJsxInvocation(
  surface: DirectTuiRetainedCommandSurface,
  invoke: (
    onDone: LocalJSXCommandOnDone,
    context: ToolUseContext & LocalJSXCommandContext,
  ) => Promise<unknown>,
): Promise<CommandInvocation> {
  let completionCalled = false
  let stateUpdateObserved = false
  let metaMessages: readonly string[] = []

  const onDone: LocalJSXCommandOnDone = (_result, options) => {
    completionCalled = true
    metaMessages = options?.metaMessages ?? []
  }
  const narrowContext = {
    abortController: new AbortController(),
    getAppState: surface.getAppState,
    setAppState: (updater: (previous: AppState) => AppState) => {
      stateUpdateObserved = true
      surface.setAppState(updater)
    },
    messages: [...surface.getMessages()],
  }

  // The four retained owners use exactly the fields above. Keeping this
  // adapter narrow prevents renderer callbacks from becoming a generic
  // ToolUseContext transport.
  await invoke(
    onDone,
    narrowContext as unknown as ToolUseContext & LocalJSXCommandContext,
  )

  return { completionCalled, stateUpdateObserved, metaMessages }
}
