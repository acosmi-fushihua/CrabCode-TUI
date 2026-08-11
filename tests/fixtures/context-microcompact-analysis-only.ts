import { mock } from 'bun:test'

import type { CachedMCState } from '../../src/services/compact/cachedMicrocompact.js'
import type { Message } from '../../src/types/message.js'

const previousSimple = process.env.CRABCODE_SIMPLE
const previousNodeEnv = process.env.NODE_ENV
process.env.CRABCODE_SIMPLE = '1'
process.env.NODE_ENV = 'test'

const { _setFeatureOverrideForTests } =
  await import('../../src/utils/featurePolyfill.js')
_setFeatureOverrideForTests('CACHED_MICROCOMPACT', true)
_setFeatureOverrideForTests('PROMPT_CACHE_BREAK_DETECTION', true)

let stateCreations = 0
let cacheEditPlans = 0
let cacheDeletionNotifications = 0
const createdStates: CachedMCState[] = []

mock.module('../../src/services/compact/cachedMicrocompact.js', () => ({
  isCachedMicrocompactEnabled: () => true,
  isModelSupportedForCacheEditing: () => true,
  getCachedMCConfig: () => ({
    supportedModels: ['fixture-model'],
    triggerThreshold: 3,
    keepRecent: 1,
  }),
  createCachedMCState: (): CachedMCState => {
    stateCreations += 1
    const state: CachedMCState = {
      registeredTools: new Set(),
      toolOrder: [],
      deletedRefs: new Set(),
      pinnedEdits: [],
    }
    createdStates.push(state)
    return state
  },
  markToolsSentToAPI: () => {},
  resetCachedMCState: (state: CachedMCState) => {
    state.registeredTools.clear()
    state.toolOrder = []
    state.deletedRefs.clear()
    state.pinnedEdits = []
  },
  registerToolResult: (state: CachedMCState, toolUseId: string) => {
    state.registeredTools.add(toolUseId)
    state.toolOrder.push(toolUseId)
  },
  registerToolMessage: () => {},
  getToolResultsToDelete: (state: CachedMCState) =>
    state.toolOrder.length > 3 ? state.toolOrder.slice(0, -1) : [],
  createCacheEditsBlock: (state: CachedMCState, toolUseIds: string[]) => {
    cacheEditPlans += 1
    for (const id of toolUseIds) state.deletedRefs.add(id)
    return {
      type: 'cache_edits' as const,
      edits: toolUseIds.map(cache_reference => ({
        type: 'delete' as const,
        cache_reference,
      })),
    }
  },
}))

const realCacheBreakDetection =
  await import('../../src/services/api/promptCacheBreakDetection.js')
mock.module('../../src/services/api/promptCacheBreakDetection.js', () => ({
  ...realCacheBreakDetection,
  notifyCacheDeletion: () => {
    cacheDeletionNotifications += 1
  },
}))

const realTokenEstimation = await import('../../src/services/tokenEstimation.js')
mock.module('../../src/services/tokenEstimation.js', () => ({
  ...realTokenEstimation,
  countMessagesTokensWithAPI: async () => 97,
  countTokensViaFastModeFallback: async () => 97,
}))

const realPrompts = await import('../../src/constants/prompts.js')
mock.module('../../src/constants/prompts.js', () => ({
  ...realPrompts,
  getSystemPrompt: async () => [],
}))

const realContext = await import('../../src/context.js')
mock.module('../../src/context.js', () => ({
  ...realContext,
  getSystemContext: async () => ({}),
}))

const realSkillPrompt = await import('../../src/tools/SkillTool/prompt.js')
mock.module('../../src/tools/SkillTool/prompt.js', () => ({
  ...realSkillPrompt,
  getLimitedSkillToolCommands: async () => [],
  getSkillToolInfo: async () => ({
    totalCommands: 0,
    includedCommands: 0,
  }),
}))

const realSkills = await import('../../src/skills/loadSkillsDir.js')
mock.module('../../src/skills/loadSkillsDir.js', () => ({
  ...realSkills,
  estimateSkillFrontmatterTokens: () => 0,
}))

const {
  consumePendingCacheEdits,
  getPinnedCacheEdits,
  microcompactMessages,
  resetMicrocompactState,
} = await import('../../src/services/compact/microCompact.js')
const { compactWarningStore } =
  await import('../../src/services/compact/compactWarningState.js')
const { collectContextData } =
  await import('../../src/commands/context/context-noninteractive.js')
const { getDefaultAppState } = await import('../../src/state/AppStateStore.js')
const { createAssistantMessage, createUserMessage } =
  await import('../../src/utils/messages.js')

const messages: Message[] = []
for (let index = 0; index < 4; index += 1) {
  const id = `fixture-tool-${index}`
  messages.push(
    createAssistantMessage({
      content: [
        {
          type: 'tool_use',
          id,
          name: 'Read',
          input: { file_path: `/fixture/${index}.txt` },
        },
      ],
    }),
    createUserMessage({
      content: [
        {
          type: 'tool_result',
          tool_use_id: id,
          content: 'x'.repeat(8_192),
        },
      ],
    }),
  )
}
const transcriptBefore = structuredClone(messages)
const appState = getDefaultAppState()
appState.agentDefinitions = { activeAgents: [], allAgents: [] }

resetMicrocompactState()
compactWarningStore.setState(() => true)
const pendingBefore = consumePendingCacheEdits()
const pinnedBefore = getPinnedCacheEdits()

const data = await collectContextData({
  messages,
  getAppState: () => appState,
  options: {
    mainLoopModel: 'fixture-model',
    tools: [],
    agentDefinitions: appState.agentDefinitions,
  },
})

const analysisStateCreations = stateCreations
const analysisCacheEditPlans = cacheEditPlans
const analysisCacheDeletionNotifications = cacheDeletionNotifications
const warningAfterAnalysis = compactWarningStore.getState()
const pendingAfterAnalysis = consumePendingCacheEdits()
const pinnedAfterAnalysis = getPinnedCacheEdits()
const transcriptUnchanged = JSON.stringify(messages) === JSON.stringify(transcriptBefore)
const analysisRegisteredToolCounts = createdStates.map(
  state => state.registeredTools.size,
)

// A production call is the control: the same over-threshold fixture must
// initialize the real global state and queue edits. This proves the preceding
// two analysis calls took the active cached-MC branch rather than passing only
// because the feature/module gate was disabled.
await microcompactMessages(messages, undefined, 'repl_main_thread')
const stateCreationsAfterProductionControl = stateCreations
const productionPendingEdits = consumePendingCacheEdits()
const productionCacheDeletionNotifications = cacheDeletionNotifications
resetMicrocompactState()

if (previousSimple === undefined) delete process.env.CRABCODE_SIMPLE
else process.env.CRABCODE_SIMPLE = previousSimple
if (previousNodeEnv === undefined) delete process.env.NODE_ENV
else process.env.NODE_ENV = previousNodeEnv

process.stdout.write(
  JSON.stringify({
    pendingBefore,
    pinnedBefore,
    analysisStateCreations,
    analysisCacheEditPlans,
    analysisCacheDeletionNotifications,
    warningAfterAnalysis,
    pendingAfterAnalysis,
    pinnedAfterAnalysis,
    transcriptUnchanged,
    analysisRegisteredToolCounts,
    stateCreationsAfterProductionControl,
    productionPendingEdits,
    productionCacheDeletionNotifications,
    analyzedModel: data.model,
  }),
)
