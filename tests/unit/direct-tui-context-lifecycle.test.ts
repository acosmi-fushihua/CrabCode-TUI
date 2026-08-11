import { describe, expect, test } from 'bun:test'
import { join } from 'node:path'

import {
  collectContextData,
  type CollectContextDataDependencies,
  handleGetContextUsageControlRequest,
} from '../../src/commands/context/context-noninteractive.js'
import {
  SDKControlGetContextUsageResponseSchema,
  SDKControlResponseSchema,
} from '../../src/entrypoints/sdk/controlSchemas.js'
import { getDefaultAppState } from '../../src/state/AppStateStore.js'
import type { Tools } from '../../src/Tool.js'
import type { AgentDefinitionsResult } from '../../src/tools/AgentTool/loadAgentsDir.js'
import type { Message } from '../../src/types/message.js'
import type {
  ContextData,
  analyzeContextUsage,
} from '../../src/utils/analyzeContext.js'
import {
  createCompactBoundaryMessage,
  createUserMessage,
  getMessagesAfterCompactBoundary,
} from '../../src/utils/messages.js'

function contextData(model = 'fixture-model'): ContextData {
  return {
    categories: [
      { name: 'Messages', tokens: 2_000, color: 'purple_FOR_SUBAGENTS_ONLY' },
      { name: 'Free space', tokens: 8_000, color: 'promptBorder' },
    ],
    totalTokens: 2_000,
    maxTokens: 10_000,
    rawMaxTokens: 10_000,
    percentage: 20,
    gridRows: [
      [
        {
          color: 'purple_FOR_SUBAGENTS_ONLY',
          isFilled: true,
          categoryName: 'Messages',
          tokens: 2_000,
          percentage: 20,
          squareFullness: 1,
        },
        {
          color: 'promptBorder',
          isFilled: true,
          categoryName: 'Free space',
          tokens: 8_000,
          percentage: 80,
          squareFullness: 1,
        },
      ],
    ],
    model,
    memoryFiles: [],
    mcpTools: [],
    agents: [],
    isAutoCompactEnabled: true,
    apiUsage: null,
  }
}

function dynamicAgent(agentType: string) {
  return {
    agentType,
    whenToUse: `Use ${agentType}`,
    source: 'projectSettings' as const,
    getSystemPrompt: () => `${agentType} prompt`,
  }
}

describe('/context collection lifecycle', () => {
  test('cuts at the latest compact boundary, microcompacts before one read-only collapse projection, and preserves the original API-usage view', async () => {
    const before = createUserMessage({ content: 'must-be-cut' })
    const boundary = createCompactBoundaryMessage(
      'manual',
      7_000,
      before.uuid,
      undefined,
      1,
    )
    const summary = createUserMessage({
      content: 'compact-summary',
      isCompactSummary: true,
      isVisibleInTranscriptOnly: true,
    })
    const after = createUserMessage({ content: 'after-boundary' })
    const projected = createUserMessage({ content: 'projected-view' })
    const microcompacted = createUserMessage({ content: 'microcompact-input' })
    const messages: Message[] = [before, boundary, summary, after]
    const originalTranscript = structuredClone(messages)

    const tools = [{ name: 'snapshot-tool' }] as unknown as Tools
    const agentDefinitions: AgentDefinitionsResult = {
      activeAgents: [dynamicAgent('snapshot-agent')],
      allAgents: [dynamicAgent('snapshot-agent')],
    }
    const appState = getDefaultAppState()
    appState.agentDefinitions = agentDefinitions
    const observed: {
      projectInput?: Message[]
      microcompactInput?: Message[]
      microcompactModel?: string
      analyzerArgs?: Parameters<typeof analyzeContextUsage>
      projectionCalls: number
    } = { projectionCalls: 0 }

    const analyzer = (async (
      ...args: Parameters<typeof analyzeContextUsage>
    ) => {
      observed.analyzerArgs = args
      return contextData()
    }) as typeof analyzeContextUsage
    const dependencies: CollectContextDataDependencies = {
      getMessagesAfterCompactBoundary,
      isContextCollapseEnabled: () => true,
      projectContextView: input => {
        observed.projectionCalls += 1
        observed.projectInput = input
        expect(input).not.toContain(projected)
        return [...input, projected]
      },
      microcompactMessages: async (input, mainLoopModel) => {
        observed.microcompactInput = input
        observed.microcompactModel = mainLoopModel
        return { messages: [microcompacted] }
      },
      analyzeContextUsage: analyzer,
    }

    await collectContextData(
      {
        messages,
        getAppState: () => appState,
        options: {
          mainLoopModel: 'snapshot-model',
          tools,
          agentDefinitions,
          customSystemPrompt: 'custom-snapshot',
          appendSystemPrompt: 'append-snapshot',
        },
      },
      dependencies,
    )

    expect(observed.microcompactInput).toEqual([boundary, summary, after])
    expect(observed.microcompactInput).not.toContain(before)
    expect(observed.microcompactInput).not.toContain(projected)
    expect(observed.microcompactModel).toBe('snapshot-model')
    expect(observed.projectionCalls).toBe(1)
    expect(observed.projectInput).toEqual([microcompacted])
    const analyzerArgs = observed.analyzerArgs
    expect(analyzerArgs?.[0]).toEqual([microcompacted, projected])
    expect(analyzerArgs?.[1]).toBe('snapshot-model')
    expect(analyzerArgs?.[3]).toBe(tools)
    expect(analyzerArgs?.[4]).toBe(agentDefinitions)
    expect(analyzerArgs?.[6]).toEqual({
      options: {
        customSystemPrompt: 'custom-snapshot',
        appendSystemPrompt: 'append-snapshot',
      },
    })
    expect(analyzerArgs?.[8]).toEqual([boundary, summary, after])
    expect(await analyzerArgs?.[2]()).toBe(appState.toolPermissionContext)
    expect(messages).toEqual(originalTranscript)
  })

  test('takes a fresh atomic tool/agent/app-state snapshot for every correlated request', async () => {
    const messages = [createUserMessage({ content: 'unchanged transcript' })]
    const stateA = getDefaultAppState()
    const stateB = getDefaultAppState()
    stateA.agentDefinitions = {
      activeAgents: [dynamicAgent('agent-a')],
      allAgents: [dynamicAgent('agent-a')],
    }
    stateB.agentDefinitions = {
      activeAgents: [dynamicAgent('agent-b')],
      allAgents: [dynamicAgent('agent-b')],
    }
    const toolA = [{ name: 'tool-a' }] as unknown as Tools
    const toolB = [{ name: 'tool-b' }] as unknown as Tools
    let current = stateA
    let snapshotReads = 0
    const captured: Array<{
      appState: ReturnType<typeof getDefaultAppState>
      tools: Tools
      agents: AgentDefinitionsResult
    }> = []

    const collect = async (
      input: Parameters<typeof collectContextData>[0],
    ): Promise<ContextData> => {
      captured.push({
        appState: input.getAppState(),
        tools: input.options.tools,
        agents: input.options.agentDefinitions,
      })
      return {
        ...contextData(input.options.mainLoopModel),
        // The public schema must strip fields that are not part of the
        // get_context_usage response rather than leaking implementation data.
        internalFixtureOnly: true,
      } as ContextData
    }
    const run = (requestId: string) =>
      handleGetContextUsageControlRequest(
        {
          message: {
            request_id: requestId,
          },
          messages,
          getAppState: () => {
            snapshotReads += 1
            return current
          },
          getMainLoopModel: () =>
            current === stateA ? 'model-a' : 'model-b',
          buildTools: snapshot => (snapshot === stateA ? toolA : toolB),
        },
        { collectContextData: collect as typeof collectContextData },
      )

    const first = await run('context-a')
    current = stateB
    const second = await run('context-b')

    expect(snapshotReads).toBe(2)
    expect(captured).toEqual([
      { appState: stateA, tools: toolA, agents: stateA.agentDefinitions },
      { appState: stateB, tools: toolB, agents: stateB.agentDefinitions },
    ])
    expect(first).toMatchObject({
      type: 'control_response',
      response: {
        subtype: 'success',
        request_id: 'context-a',
        response: { model: 'model-a' },
      },
    })
    expect(second).toMatchObject({
      type: 'control_response',
      response: {
        subtype: 'success',
        request_id: 'context-b',
        response: { model: 'model-b' },
      },
    })
    expect(
      (first.response as { response?: Record<string, unknown> }).response,
    ).not.toHaveProperty('internalFixtureOnly')
    expect(() => SDKControlResponseSchema().parse(first)).not.toThrow()
    if (first.response.subtype !== 'success') {
      throw new Error('fixture expected a success response')
    }
    expect(() =>
      SDKControlGetContextUsageResponseSchema().parse(first.response.response),
    ).not.toThrow()
  })

  test('correlates collection and schema failures without mutating established state', async () => {
    const messages = [createUserMessage({ content: 'retained transcript' })]
    const appState = getDefaultAppState()
    const beforeMessages = structuredClone(messages)
    const beforeAgentDefinitions = appState.agentDefinitions

    const collectionFailure = await handleGetContextUsageControlRequest(
      {
        message: { request_id: 'context-collection-error' },
        messages,
        getAppState: () => appState,
        getMainLoopModel: () => 'fixture-model',
        buildTools: () => [],
      },
      {
        collectContextData: async () => {
          throw new Error('fixture context collection failed')
        },
      },
    )
    expect(collectionFailure).toEqual({
      type: 'control_response',
      response: {
        subtype: 'error',
        request_id: 'context-collection-error',
        error: 'fixture context collection failed',
      },
    })

    const schemaFailure = await handleGetContextUsageControlRequest(
      {
        message: { request_id: 'context-schema-error' },
        messages,
        getAppState: () => appState,
        getMainLoopModel: () => 'fixture-model',
        buildTools: () => [],
      },
      {
        collectContextData: async () =>
          ({ ...contextData(), rawMaxTokens: 'not-a-number' }) as never,
      },
    )
    expect(() => SDKControlResponseSchema().parse(schemaFailure)).not.toThrow()
    expect(schemaFailure.type).toBe('control_response')
    expect(schemaFailure.response.subtype).toBe('error')
    expect(schemaFailure.response.request_id).toBe('context-schema-error')
    if (schemaFailure.response.subtype !== 'error') {
      throw new Error('fixture expected an error response')
    }
    expect(schemaFailure.response.error).toContain('rawMaxTokens')
    expect(messages).toEqual(beforeMessages)
    expect(appState.agentDefinitions).toBe(beforeAgentDefinitions)
  })

  test('runs both cached-microcompact analysis passes without committing global lifecycle state', async () => {
    const fixture = join(
      import.meta.dir,
      '..',
      'fixtures',
      'context-microcompact-analysis-only.ts',
    )
    const child = Bun.spawn([process.execPath, fixture], {
      cwd: join(import.meta.dir, '..', '..'),
      env: { ...process.env, NODE_ENV: 'test' },
      stdout: 'pipe',
      stderr: 'pipe',
    })
    const [exitCode, stdout, stderr] = await Promise.all([
      child.exited,
      new Response(child.stdout).text(),
      new Response(child.stderr).text(),
    ])
    expect(exitCode, stderr).toBe(0)

    const result = JSON.parse(stdout) as {
      pendingBefore: unknown
      pinnedBefore: unknown[]
      analysisStateCreations: number
      analysisCacheEditPlans: number
      analysisCacheDeletionNotifications: number
      warningAfterAnalysis: boolean
      pendingAfterAnalysis: unknown
      pinnedAfterAnalysis: unknown[]
      transcriptUnchanged: boolean
      analysisRegisteredToolCounts: number[]
      stateCreationsAfterProductionControl: number
      productionPendingEdits: {
        type: string
        edits: Array<{ cache_reference: string }>
      } | null
      productionCacheDeletionNotifications: number
      analyzedModel: string
    }

    expect(result).toMatchObject({
      pendingBefore: null,
      pinnedBefore: [],
      analysisStateCreations: 2,
      analysisCacheEditPlans: 2,
      analysisCacheDeletionNotifications: 0,
      warningAfterAnalysis: true,
      pendingAfterAnalysis: null,
      pinnedAfterAnalysis: [],
      transcriptUnchanged: true,
      analysisRegisteredToolCounts: [4, 4],
      stateCreationsAfterProductionControl: 3,
      productionCacheDeletionNotifications: 1,
      analyzedModel: 'fixture-model',
    })
    expect(result.productionPendingEdits).toEqual({
      type: 'cache_edits',
      edits: [
        { type: 'delete', cache_reference: 'fixture-tool-0' },
        { type: 'delete', cache_reference: 'fixture-tool-1' },
        { type: 'delete', cache_reference: 'fixture-tool-2' },
      ],
    })
  })
})
