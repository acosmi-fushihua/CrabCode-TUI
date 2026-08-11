/**
 * Command inventory for process-owned StructuredIO runtimes.
 *
 * The interactive registry intentionally imports every terminal command,
 * including React/Ink dialogs. Generic process-owned runtimes can execute only
 * prompt commands and `supportsNonInteractive` local commands. The direct TUI
 * additionally projects a fixed, reviewed pair of renderer-neutral historical
 * locals without mutating their shared metadata. This module rebuilds the same
 * first-wins discovery pipeline without coupling the backend to Ink.
 */

import memoize from 'lodash-es/memoize.js'
import clear from 'src/commands/clear/index.js'
import compact from 'src/commands/compact/index.js'
import compactHistory from 'src/commands/compact-history/index.js'
import context from 'src/commands/context/index.js'
import cost from 'src/commands/cost/index.js'
import advisor from 'src/commands/advisor.js'
import commit from 'src/commands/commit.js'
import commitPushPr from 'src/commands/commit-push-pr.js'
import extraUsage from 'src/commands/extra-usage/index.js'
import files from 'src/commands/files/index.js'
import heapDump from 'src/commands/heapdump/index.js'
import init from 'src/commands/init.js'
import initVerifiers from 'src/commands/init-verifiers.js'
import insights from 'src/commands/insights/index.js'
import installSlackApp from 'src/commands/install-slack-app/index.js'
import localModels from 'src/commands/local-models/index.js'
import outputStyle from 'src/commands/output-style/index.js'
import prComments from 'src/commands/pr_comments/index.js'
import proactive from 'src/commands/proactive.js'
import proxy from 'src/commands/proxy/index.js'
import releaseNotes from 'src/commands/release-notes/index.js'
import review from 'src/commands/reviewCommand.js'
import securityReview from 'src/commands/security-review.js'
import smallModel from 'src/commands/smallmodel/index.js'
import { directTuiStatusline } from 'src/commands/statusline.js'
import terminalSetup from 'src/commands/terminalSetup/index.js'
import version from 'src/commands/version.js'
import vision from 'src/commands/vision/index.js'
import workflows from 'src/commands/workflows/index.js'
import { getBundledSkills } from 'src/skills/bundledSkills.js'
import {
  clearSkillCaches,
  getDynamicSkills,
  getSkillDirCommands,
} from 'src/skills/loadSkillsDir.js'
import { getBuiltinPluginSkillCommands } from 'src/plugins/builtinPlugins.js'
import {
  clearPluginCommandCache,
  clearPluginSkillsCache,
  getPluginCommands,
  getPluginSkills,
} from 'src/utils/plugins/loadPluginCommands.js'
import { getWorkflowCommands } from 'src/tools/WorkflowTool/createWorkflowCommand.js'
import { dedupeCommandsByStableName } from 'src/utils/commandDedupe.js'
import { logError } from 'src/utils/log.js'
import { logForDebugging } from 'src/utils/debug.js'
import { toError } from 'src/utils/errors.js'
import {
  type Command,
  isCommandEnabled,
} from 'src/types/command.js'
import { isAcosmiSubscriber, isUsing3PServices } from 'src/utils/auth.js'
import { isFirstPartyAcosmiBaseUrl } from 'src/utils/model/providers.js'
import { localizeCommandDescription } from 'src/i18n/catalogLocalization.js'
import { getSettingSourceName } from 'src/utils/settings/constants.js'
import { filterSkillToolCommands } from 'src/utils/skillCommandCatalog.js'
import { installSkillCommandCatalogProvider } from 'src/utils/skillCommandCatalogProvider.js'
import { installActiveCommandCacheInvalidator } from 'src/utils/commandCacheInvalidation.js'
import { installBuiltInCommandNamesProvider } from 'src/utils/builtInCommandNamesProvider.js'
import { installActiveCommandLoader } from 'src/utils/activeCommandInventory.js'
import { feature } from 'src/utils/featurePolyfill.js'
import { isWorkflowRuntimeEnabled } from 'src/utils/workflowRuntimeEnabled.js'

const ANT_RENDERER_NEUTRAL_BUILTINS: readonly Command[] =
  process.env.USER_TYPE === 'ant' && !process.env.IS_DEMO
    ? [commit, commitPushPr, initVerifiers, version]
    : []

// `/workflows` is the plugin-workflow management/entry surface and therefore
// stays behind the broader preview flag. Bundled workflows such as
// `/deep-research` are discovered separately whenever the narrower runtime is
// enabled; enabling that bundled runtime must not expose plugin scripts.
const WORKFLOW_MANAGEMENT_BUILTINS: readonly Command[] = feature(
  'WORKFLOW_SCRIPTS',
)
  ? [workflows]
  : []

/**
 * Exact current-HEAD COMMANDS projection after removing local-jsx commands
 * and commands that explicitly prohibit non-interactive execution.
 *
 * Order is observable: it controls built-in catalog order and preserves the
 * original first-wins discovery semantics.
 */
const HEADLESS_BUILTINS: readonly Command[] = [
  advisor,
  compact,
  compactHistory,
  context,
  cost,
  files,
  heapDump,
  init,
  localModels,
  prComments,
  proxy,
  releaseNotes,
  review,
  securityReview,
  extraUsage,
  insights,
  ...WORKFLOW_MANAGEMENT_BUILTINS,
  ...ANT_RENDERER_NEUTRAL_BUILTINS,
]

/**
 * The fixed direct Ink registry owned these renderer-neutral local actions,
 * while their shared command metadata deliberately marked them unavailable
 * to generic non-interactive callers. Project only the reviewed actions
 * onto the process-private direct TUI route; do not mutate the shared command
 * definition or widen the print/SDK catalog.
 */
function projectDirectRendererNeutralLocal(command: Command): Command {
  if (command.type !== 'local') {
    throw new Error(
      `direct renderer-neutral projection requires a local command: ${command.name}`,
    )
  }
  // Object spread invokes accessors. Several historical command descriptions
  // are intentionally lazy, and evaluating one while this registry's module
  // graph is still initializing can enter an incomplete dependency cycle.
  // Copy descriptors so the direct-only metadata override preserves those
  // getters and does not change when backend/model state is observed.
  const projected = Object.create(
    Object.getPrototypeOf(command),
  ) as Command
  Object.defineProperties(
    projected,
    Object.getOwnPropertyDescriptors(command),
  )
  Object.defineProperty(projected, 'supportsNonInteractive', {
    configurable: true,
    enumerable: true,
    value: true,
    writable: true,
  })
  return projected
}

const DIRECT_TUI_INSTALL_SLACK_APP =
  projectDirectRendererNeutralLocal(installSlackApp)
const DIRECT_TUI_PROACTIVE =
  projectDirectRendererNeutralLocal(proactive)
const DIRECT_TUI_CLEAR =
  projectDirectRendererNeutralLocal(clear)
const DIRECT_TUI_SMALLMODEL =
  projectDirectRendererNeutralLocal(smallModel)
const DIRECT_TUI_VISION = projectDirectRendererNeutralLocal(vision)

/**
 * The terminal setup implementation already performs only backend/local-OS
 * work and completes through onDone. Its exact route-private adapter captures
 * that result as a worker-slash text result; no other local-jsx command gains
 * direct authority.
 */
const DIRECT_TUI_TERMINAL_SETUP = {
  ...terminalSetup,
  type: 'local',
  supportsNonInteractive: true,
  load: () => import('src/commands/terminalSetup/direct.js'),
} satisfies Command

/**
 * The fixed `/output-style` module is nominally `local-jsx`, but its exact
 * action returns no JSX: it synchronously completes through `onDone` with the
 * historical system result. Keep this object-identity allowlist private to the
 * interactive direct route instead of broadening the local-jsx protocol.
 */
const DIRECT_TUI_RENDERER_NEUTRAL_LOCAL_JSX = new Set<Command>([
  outputStyle,
])

function isDirectTuiRendererNeutralLocalJsx(command: Command): boolean {
  return (
    command.type === 'local-jsx' &&
    DIRECT_TUI_RENDERER_NEUTRAL_LOCAL_JSX.has(command)
  )
}

/**
 * Interactive direct-TUI projection.
 *
 * `/statusline` was historically disabled only for print/SDK sessions and
 * `/logout` was historically a TUI command available only outside 3P provider
 * mode. They are renderer-neutral once invoked, but must not broaden the
 * generic headless surface.
 */
const DIRECT_TUI_BUILTINS: readonly Command[] = [
  advisor,
  DIRECT_TUI_CLEAR,
  compact,
  compactHistory,
  cost,
  files,
  heapDump,
  DIRECT_TUI_INSTALL_SLACK_APP,
  init,
  localModels,
  DIRECT_TUI_SMALLMODEL,
  outputStyle,
  prComments,
  proxy,
  releaseNotes,
  directTuiStatusline,
  review,
  securityReview,
  DIRECT_TUI_TERMINAL_SETUP,
  extraUsage,
  insights,
  DIRECT_TUI_VISION,
  ...WORKFLOW_MANAGEMENT_BUILTINS,
  ...(feature('PROACTIVE') || feature('KAIROS')
    ? [DIRECT_TUI_PROACTIVE]
    : []),
  ...ANT_RENDERER_NEUTRAL_BUILTINS,
]

/**
 * These invocation names are owned by the Rust renderer in the direct TUI.
 * Exclude a discovered command that claims either canonical name or an alias
 * so runtime discovery cannot silently shadow the renderer's control-backed
 * lifecycle.
 */
const DIRECT_TUI_RENDERER_OWNED_INVOCATIONS = new Set([
  'logout',
  'reload-plugins',
])

function claimsDirectTuiRendererOwnedInvocation(command: Command): boolean {
  return [command.name, ...(command.aliases ?? [])].some(name =>
    DIRECT_TUI_RENDERER_OWNED_INVOCATIONS.has(name),
  )
}

const HEADLESS_BUILTIN_NAMES = new Set(
  HEADLESS_BUILTINS.flatMap(command => [
    command.name,
    ...(command.aliases ?? []),
  ]),
)

const DIRECT_TUI_BUILTIN_NAMES = new Set(
  DIRECT_TUI_BUILTINS.flatMap(command => [
    command.name,
    ...(command.aliases ?? []),
  ]),
)

const HEADLESS_SUBSCRIBER_GATED_NAMES = new Set(
  HEADLESS_BUILTINS.filter(command =>
    command.availability?.includes('crabcode-ai'),
  ).flatMap(command => [command.name, ...(command.aliases ?? [])]),
)

export function getDirectTuiBuiltInCommandNames(): ReadonlySet<string> {
  return DIRECT_TUI_BUILTIN_NAMES
}

/**
 * Return the exact built-in command objects that feed the direct-TUI catalog.
 *
 * This is intentionally the pre-availability inventory: callers that execute
 * commands must continue to use `getDirectTuiCommands()`.  Keeping a read-only
 * view of the actual objects lets release audits bind every advertised token
 * to its production execution kind without evaluating account, feature, or
 * network gates and without maintaining a second hand-written registry.
 */
export function getDirectTuiBuiltInCommandDefinitions(): readonly Command[] {
  return DIRECT_TUI_BUILTINS
}

export function installDirectTuiCommandSurface(): void {
  installBuiltInCommandNamesProvider(() => DIRECT_TUI_BUILTIN_NAMES)
  installActiveCommandLoader(getDirectTuiCommands)
}

export function installHeadlessCommandSurface(): void {
  installBuiltInCommandNamesProvider(() => HEADLESS_BUILTIN_NAMES)
  installActiveCommandLoader(getHeadlessCommands)
}

export function isHeadlessBuiltInCommandName(name: string): boolean {
  return HEADLESS_BUILTIN_NAMES.has(name)
}

export function getHeadlessBuiltInCommandNames(): ReadonlySet<string> {
  return HEADLESS_BUILTIN_NAMES
}

export function isHeadlessSubscriberGatedCommandName(
  name: string,
): boolean {
  return HEADLESS_SUBSCRIBER_GATED_NAMES.has(name)
}

export function getHeadlessSubscriberGatedCommandNames(): ReadonlySet<string> {
  return HEADLESS_SUBSCRIBER_GATED_NAMES
}

export function meetsHeadlessAvailabilityRequirement(
  command: Command,
): boolean {
  if (!command.availability) return true
  for (const availability of command.availability) {
    if (availability === 'crabcode-ai' && isAcosmiSubscriber()) return true
    if (
      availability === 'console' &&
      !isAcosmiSubscriber() &&
      !isUsing3PServices() &&
      isFirstPartyAcosmiBaseUrl()
    ) {
      return true
    }
  }
  return false
}

async function getSkills(cwd: string): Promise<{
  skillDirCommands: Command[]
  pluginSkills: Command[]
  bundledSkills: Command[]
  builtinPluginSkills: Command[]
}> {
  try {
    const [skillDirCommands, pluginSkills] = await Promise.all([
      getSkillDirCommands(cwd).catch(error => {
        logError(toError(error))
        return []
      }),
      getPluginSkills().catch(error => {
        logError(toError(error))
        return []
      }),
    ])
    return {
      skillDirCommands,
      pluginSkills,
      bundledSkills: getBundledSkills(),
      builtinPluginSkills: getBuiltinPluginSkillCommands(),
    }
  } catch (error) {
    logError(toError(error))
    return {
      skillDirCommands: [],
      pluginSkills: [],
      bundledSkills: [],
      builtinPluginSkills: [],
    }
  }
}

function createCommandLoader(
  builtins: readonly Command[],
) {
  return memoize(async (cwd: string): Promise<Command[]> => {
    const [
      {
        skillDirCommands,
        pluginSkills,
        bundledSkills,
        builtinPluginSkills,
      },
      pluginCommands,
      workflowCommands,
    ] = await Promise.all([
      getSkills(cwd),
      getPluginCommands(),
      isWorkflowRuntimeEnabled()
        ? Promise.resolve(getWorkflowCommands(cwd))
        : Promise.resolve([]),
    ])

    return dedupeCommandsByStableName(
      [
        { source: 'bundled skills', commands: bundledSkills },
        { source: 'builtin plugin skills', commands: builtinPluginSkills },
        { source: 'skill directory', commands: skillDirCommands },
        { source: 'workflow commands', commands: workflowCommands },
        { source: 'plugin commands', commands: pluginCommands },
        { source: 'plugin skills', commands: pluginSkills },
        { source: 'built-in commands', commands: [...builtins] },
      ],
      logForDebugging,
    )
  })
}

const loadAllHeadlessCommands = createCommandLoader(HEADLESS_BUILTINS)
const loadAllDirectTuiCommands = createCommandLoader(DIRECT_TUI_BUILTINS)

async function getCommandsForSurface(
  cwd: string,
  loadCommands: (cwd: string) => Promise<Command[]>,
  builtins: readonly Command[],
  excludeCommand: (command: Command) => boolean = () => false,
  includeRendererNeutralLocalJsx: (command: Command) => boolean = () =>
    false,
): Promise<Command[]> {
  const allCommands = await loadCommands(cwd)
  const eligibleForSurface = (command: Command) =>
      !excludeCommand(command) &&
      meetsHeadlessAvailabilityRequirement(command) &&
      isCommandEnabled(command) &&
      ((command.type === 'prompt' && !command.disableNonInteractive) ||
        (command.type === 'local' && command.supportsNonInteractive) ||
        includeRendererNeutralLocalJsx(command))

  const filteredCommands = allCommands.filter(eligibleForSurface)

  // Discovery is first-wins, but eligibility is a surface concern. An
  // ineligible discovered command with a built-in's stable name must not
  // remove the healthy built-in before this surface gets a chance to filter
  // it. Restore only missing eligible built-ins; an eligible discovered
  // command continues to own the collision.
  const claimedStableNames = new Set(filteredCommands.map(command => command.name))
  const fallbackBuiltins = builtins.filter(
    command =>
      eligibleForSurface(command) && !claimedStableNames.has(command.name),
  )
  const baseCommands = [...filteredCommands, ...fallbackBuiltins]

  const dynamicSkills = getDynamicSkills()
  if (dynamicSkills.length === 0) return baseCommands

  const baseNames = new Set(baseCommands.map(command => command.name))
  const uniqueDynamicSkills = dynamicSkills.filter(
    command =>
      !excludeCommand(command) &&
      !baseNames.has(command.name) &&
      command.type === 'prompt' &&
      !command.disableNonInteractive &&
      meetsHeadlessAvailabilityRequirement(command) &&
      isCommandEnabled(command),
  )
  const builtInNames = new Set(builtins.map(command => command.name))
  const insertIndex = baseCommands.findIndex(command =>
    builtInNames.has(command.name),
  )
  if (insertIndex === -1) {
    return [...baseCommands, ...uniqueDynamicSkills]
  }
  return [
    ...baseCommands.slice(0, insertIndex),
    ...uniqueDynamicSkills,
    ...baseCommands.slice(insertIndex),
  ]
}

export function getHeadlessCommands(cwd: string): Promise<Command[]> {
  return getCommandsForSurface(
    cwd,
    loadAllHeadlessCommands,
    HEADLESS_BUILTINS,
  )
}

export function getDirectTuiCommands(cwd: string): Promise<Command[]> {
  return getCommandsForSurface(
    cwd,
    loadAllDirectTuiCommands,
    DIRECT_TUI_BUILTINS,
    claimsDirectTuiRendererOwnedInvocation,
    isDirectTuiRendererNeutralLocalJsx,
  )
}

export const getHeadlessSlashCommandToolSkills = memoize(
  async (cwd: string): Promise<Command[]> => {
    try {
      const allCommands = await getHeadlessCommands(cwd)
      return allCommands.filter(
        command =>
          command.type === 'prompt' &&
          command.source !== 'builtin' &&
          (command.hasUserSpecifiedDescription || command.whenToUse) &&
          (command.loadedFrom === 'skills' ||
            command.loadedFrom === 'plugin' ||
            command.loadedFrom === 'bundled' ||
            command.disableModelInvocation),
      )
    } catch (error) {
      logError(toError(error))
      logForDebugging(
        'Returning empty headless skills array due to load failure',
      )
      return []
    }
  },
)

/**
 * Model-invocable skill catalog used by the system prompt. This is the exact
 * renderer-free counterpart of commands.ts#getSkillToolCommands.
 */
export const getHeadlessSkillToolCommands = memoize(
  async (cwd: string): Promise<Command[]> => {
    const allCommands = await getHeadlessCommands(cwd)
    return filterSkillToolCommands(allCommands)
  },
)

export function clearHeadlessCommandsCache(): void {
  loadAllHeadlessCommands.cache?.clear?.()
  loadAllDirectTuiCommands.cache?.clear?.()
  getHeadlessSkillToolCommands.cache?.clear?.()
  getHeadlessSlashCommandToolSkills.cache?.clear?.()
  clearPluginCommandCache()
  clearPluginSkillsCache()
  clearSkillCaches()
}

export function clearHeadlessCommandMemoizationCaches(): void {
  loadAllHeadlessCommands.cache?.clear?.()
  loadAllDirectTuiCommands.cache?.clear?.()
  getHeadlessSkillToolCommands.cache?.clear?.()
  getHeadlessSlashCommandToolSkills.cache?.clear?.()
}

export function formatHeadlessCommandDescription(command: Command): string {
  if (command.type !== 'prompt') return command.description

  const description = localizeCommandDescription(command)
  if (command.kind === 'workflow') return `${description} (workflow)`
  if (command.source === 'plugin') {
    const pluginName = command.pluginInfo?.pluginManifest.name
    return pluginName
      ? `(${pluginName}) ${description}`
      : `${description} (plugin)`
  }
  if (command.source === 'builtin' || command.source === 'mcp') {
    return description
  }
  if (command.source === 'bundled') return `${description} (bundled)`
  return `${description} (${getSettingSourceName(command.source)})`
}

installSkillCommandCatalogProvider({
  getSkillToolCommands: getHeadlessSkillToolCommands,
  getSlashCommandToolSkills: getHeadlessSlashCommandToolSkills,
})
installActiveCommandCacheInvalidator(clearHeadlessCommandsCache)
installHeadlessCommandSurface()
