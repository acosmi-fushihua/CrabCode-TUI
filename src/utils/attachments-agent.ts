/**
 * attachments-agent.ts -- Agent, skill, and queued-command attachment logic.
 *
 * Split from attachments.ts (3997 lines) for maintainability. Contains
 * agent pending message handling, skill listing/discovery, queued command
 * processing, and skill name tracking.
 */
import type { Base64ImageSource, ContentBlockParam, ImageBlockParam } from '../types/api-types.js'
import {
  logEvent,
} from 'src/services/analytics/index.js'
import { toolMatchesName, type ToolUseContext } from '../Tool.js'

import { type QueuedCommand, getImagePasteIds, isValidImagePaste } from 'src/types/textInputTypes.js'
import type { PastedContent } from './config.js'
import { maybeResizeAndDownsampleImageBlock } from './imageResizer.js'
import { extractTextContent } from './messages.js'
import { logForDebugging } from './debug.js'
import { SKILL_TOOL_NAME } from '../tools/SkillTool/constants.js'
import type { Command } from '../types/command.js'
import {
  filterMcpSkillCommands,
  filterSkillToolCommands,
} from './skillCommandCatalog.js'
import uniqBy from 'lodash-es/uniqBy.js'
import { formatCommandsWithinBudget } from '../tools/SkillTool/prompt.js'
import { getContextWindowForModel } from './context.js'
import { getSdkBetas } from '../bootstrap/state.js'
import { readdir, stat } from 'fs/promises'
import { relative, resolve } from 'path'
import { getCwd } from 'src/utils/cwd.js'
import { drainPendingMessages } from '../tasks/LocalAgentTask/LocalAgentTask.js'
import { feature } from './featurePolyfill.js'
/* eslint-disable @typescript-eslint/no-require-imports */
const skillSearchModules = feature('EXPERIMENTAL_SKILL_SEARCH')
  ? {
      featureCheck:
        require('../services/skillSearch/featureCheck.js') as typeof import('../services/skillSearch/featureCheck.js'),
    }
  : null
/* eslint-enable @typescript-eslint/no-require-imports */

import type { Attachment } from './attachments-types.js'
import { extractAgentMentions } from './attachments-parse.js'

const INLINE_NOTIFICATION_MODES = new Set(['prompt', 'task-notification'])

export async function getQueuedCommandAttachments(
  queuedCommands: QueuedCommand[],
): Promise<Attachment[]> {
  if (!queuedCommands) {
    return []
  }
  // Include both 'prompt' and 'task-notification' commands as attachments.
  // During proactive agentic loops, task-notification commands would otherwise
  // stay in the queue permanently (useQueueProcessor can't run while a query
  // is active), causing hasPendingNotifications() to return true and Sleep to
  // wake immediately with 0ms duration in an infinite loop.
  const filtered = queuedCommands.filter(_ =>
    INLINE_NOTIFICATION_MODES.has(_.mode),
  )
  return Promise.all(
    filtered.map(async _ => {
      const imageBlocks = await buildImageContentBlocks(_.pastedContents)
      let prompt: string | Array<ContentBlockParam> = _.value
      if (imageBlocks.length > 0) {
        // Build content block array with text + images so the model sees them
        const textValue =
          typeof _.value === 'string'
            ? _.value
            : extractTextContent(_.value, '\n')
        prompt = [{ type: 'text' as const, text: textValue }, ...imageBlocks]
      }
      return {
        type: 'queued_command' as const,
        prompt,
        source_uuid: _.uuid,
        imagePasteIds: getImagePasteIds(_.pastedContents),
        commandMode: _.mode,
        origin: _.origin,
        isMeta: _.isMeta,
      }
    }),
  )
}

export function getAgentPendingMessageAttachments(
  toolUseContext: ToolUseContext,
): Attachment[] {
  const agentId = toolUseContext.agentId
  if (!agentId) return []
  const drained = drainPendingMessages(
    agentId,
    toolUseContext.getAppState,
    toolUseContext.setAppStateForTasks ?? toolUseContext.setAppState,
  )
  return drained.map(msg => ({
    type: 'queued_command' as const,
    prompt: msg,
    origin: { kind: 'coordinator' as const },
    isMeta: true,
  }))
}

async function buildImageContentBlocks(
  pastedContents: Record<number, PastedContent> | undefined,
): Promise<ImageBlockParam[]> {
  if (!pastedContents) {
    return []
  }
  const imageContents = Object.values(pastedContents).filter(isValidImagePaste)
  if (imageContents.length === 0) {
    return []
  }
  const results = await Promise.all(
    imageContents.map(async img => {
      const imageBlock: ImageBlockParam = {
        type: 'image',
        source: {
          type: 'base64',
          media_type: (img.mediaType ||
            'image/png') as Base64ImageSource['media_type'],
          data: img.content,
        },
      }
      const resized = await maybeResizeAndDownsampleImageBlock(imageBlock)
      return resized.block
    }),
  )
  return results
}

// Track which skills have been sent to avoid re-sending. Keyed by agentId
// (empty string = main thread) so subagents get their own turn-0 listing --
// without per-agent scoping, the main thread populating this Set would cause
// every subagent's filterToBundledAndMcp result to dedup to empty.
const sentSkillNames = new Map<string, Set<string>>()

// Called when the skill set genuinely changes (plugin reload, skill file
// change on disk) so new skills get announced. NOT called on compact --
// post-compact re-injection costs ~4K tokens/event for marginal benefit.
export function resetSentSkillNames(): void {
  sentSkillNames.clear()
  suppressNext = false
}

/**
 * Suppress the next skill-listing injection. Called by conversationRecovery
 * on --resume when a skill_listing attachment already exists in the
 * transcript.
 *
 * `sentSkillNames` is module-scope -- process-local. Each `crabcode -p` spawn
 * starts with an empty Map, so without this every resume re-injects the
 * full ~600-token listing even though it's already in the conversation from
 * the prior process. Shows up on every --resume; particularly loud for
 * daemons that respawn frequently.
 *
 * Trade-off: skills added between sessions won't be announced until the
 * next non-resume session. Acceptable -- skill_listing was never meant to
 * cover cross-process deltas, and the agent can still call them (they're
 * in the Skill tool's runtime registry regardless).
 */
export function suppressNextSkillListing(): void {
  suppressNext = true
}
let suppressNext = false

// When skill-search is enabled and the filtered (bundled + MCP) listing exceeds
// this count, fall back to bundled-only. Protects MCP-heavy users (100+ servers)
// from truncation while keeping the turn-0 guarantee for typical setups.
const FILTERED_LISTING_MAX = 30

/**
 * Filter skills to bundled (Acosmi-curated) + MCP (user-connected) only.
 * Used when skill-search is enabled to resolve the turn-0 gap for subagents:
 * these sources are small, intent-signaled, and won't hit the truncation budget.
 * User/project/plugin skills (the long tail -- 200+) go through discovery instead.
 *
 * Falls back to bundled-only if bundled+mcp exceeds FILTERED_LISTING_MAX.
 */
export function filterToBundledAndMcp(commands: Command[]): Command[] {
  const filtered = commands.filter(
    cmd => cmd.loadedFrom === 'bundled' || cmd.loadedFrom === 'mcp',
  )
  if (filtered.length > FILTERED_LISTING_MAX) {
    return filtered.filter(cmd => cmd.loadedFrom === 'bundled')
  }
  return filtered
}

export async function getSkillListingAttachments(
  toolUseContext: ToolUseContext,
): Promise<Attachment[]> {
  if (process.env.NODE_ENV === 'test') {
    return []
  }

  // Skip skill listing for agents that don't have the Skill tool -- they can't use skills directly.
  if (
    !toolUseContext.options.tools.some(t => toolMatchesName(t, SKILL_TOOL_NAME))
  ) {
    return []
  }

  const localCommands = filterSkillToolCommands(
    toolUseContext.options.commands,
  )
  const mcpSkills = filterMcpSkillCommands(
    toolUseContext.getAppState().mcp.commands,
  )
  let allCommands =
    mcpSkills.length > 0
      ? uniqBy([...localCommands, ...mcpSkills], 'name')
      : localCommands

  // When skill search is active, filter to bundled + MCP instead of full
  // suppression. Resolves the turn-0 gap: main thread gets turn-0 discovery
  // via getTurnZeroSkillDiscovery (blocking), but subagents use the async
  // subagent_spawn signal (collected post-tools, visible turn 1). Bundled +
  // MCP are small and intent-signaled; user/project/plugin skills go through
  // discovery. feature() first for DCE -- the property-access string leaks
  // otherwise even with ?. on null.
  if (
    feature('EXPERIMENTAL_SKILL_SEARCH') &&
    skillSearchModules?.featureCheck.isSkillSearchEnabled()
  ) {
    allCommands = filterToBundledAndMcp(allCommands)
  }

  const agentKey = toolUseContext.agentId ?? ''
  let sent = sentSkillNames.get(agentKey)
  if (!sent) {
    sent = new Set()
    sentSkillNames.set(agentKey, sent)
  }

  // Resume path: prior process already injected a listing; it's in the
  // transcript. Mark everything current as sent so only post-resume deltas
  // (skills loaded later via /reload-plugins etc) get announced.
  if (suppressNext) {
    suppressNext = false
    for (const cmd of allCommands) {
      sent.add(cmd.name)
    }
    return []
  }

  // Find skills we haven't sent yet
  const newSkills = allCommands.filter(cmd => !sent.has(cmd.name))

  if (newSkills.length === 0) {
    return []
  }

  // If no skills have been sent yet, this is the initial batch
  const isInitial = sent.size === 0

  // Mark as sent
  for (const cmd of newSkills) {
    sent.add(cmd.name)
  }

  logForDebugging(
    `Sending ${newSkills.length} skills via attachment (${isInitial ? 'initial' : 'dynamic'}, ${sent.size} total sent)`,
  )

  // Format within budget using existing logic
  const contextWindowTokens = getContextWindowForModel(
    toolUseContext.options.mainLoopModel,
    getSdkBetas(),
  )
  const content = formatCommandsWithinBudget(newSkills, contextWindowTokens)

  return [
    {
      type: 'skill_listing',
      content,
      skillCount: newSkills.length,
      isInitial,
    },
  ]
}

// getSkillDiscoveryAttachment moved to skillSearch/prefetch.ts as
// getTurnZeroSkillDiscovery -- keeps the 'skill_discovery' string literal inside
// a feature-gated module so it doesn't leak into external builds.

/**
 * Processes skill directories that were discovered during file operations.
 * Uses dynamicSkillDirTriggers field from ToolUseContext
 */
export async function getDynamicSkillAttachments(
  toolUseContext: ToolUseContext,
): Promise<Attachment[]> {
  const attachments: Attachment[] = []

  if (
    toolUseContext.dynamicSkillDirTriggers &&
    toolUseContext.dynamicSkillDirTriggers.size > 0
  ) {
    // Parallelize: readdir all skill dirs concurrently
    const perDirResults = await Promise.all(
      Array.from(toolUseContext.dynamicSkillDirTriggers).map(async skillDir => {
        try {
          const entries = await readdir(skillDir, { withFileTypes: true })
          const candidates = entries
            .filter(e => e.isDirectory() || e.isSymbolicLink())
            .map(e => e.name)
          // Parallelize: stat all SKILL.md candidates concurrently
          const checked = await Promise.all(
            candidates.map(async name => {
              try {
                await stat(resolve(skillDir, name, 'SKILL.md'))
                return name
              } catch {
                return null // SKILL.md doesn't exist, skip this entry
              }
            }),
          )
          return {
            skillDir,
            skillNames: checked.filter((n): n is string => n !== null),
          }
        } catch {
          // Ignore errors reading skill directories (e.g., directory doesn't exist)
          return { skillDir, skillNames: [] }
        }
      }),
    )

    for (const { skillDir, skillNames } of perDirResults) {
      if (skillNames.length > 0) {
        attachments.push({
          type: 'dynamic_skill',
          skillDir,
          skillNames,
          displayPath: relative(getCwd(), skillDir),
        })
      }
    }

    toolUseContext.dynamicSkillDirTriggers.clear()
  }

  return attachments
}

export function processAgentMentions(
  input: string,
  agents: import('../tools/AgentTool/loadAgentsDir.js').AgentDefinition[],
): Attachment[] {
  const agentMentions = extractAgentMentions(input)
  if (agentMentions.length === 0) return []

  const results = agentMentions.map(mention => {
    const agentType = mention.replace('agent-', '')
    const agentDef = agents.find(def => def.agentType === agentType)

    if (!agentDef) {
      logEvent('tengu_at_mention_agent_not_found', {})
      return null
    }

    logEvent('tengu_at_mention_agent_success', {})

    return {
      type: 'agent_mention' as const,
      agentType: agentDef.agentType,
    }
  })

  return results.filter(
    (result): result is NonNullable<typeof result> => result !== null,
  )
}
