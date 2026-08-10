import { z } from 'zod/v4'
import {
  createTaskStateBase,
  generateTaskId,
} from '../../Task.js'
import {
  buildTool,
  type ToolDef,
  type ToolUseContext,
} from '../../Tool.js'
import type { WorkflowToolProgress } from '../../types/tools.js'
import {
  localizeWorkflowDescription,
  localizeWorkflowPhase,
} from '../../i18n/catalogLocalization.js'
import { createChildAbortController } from '../../utils/abortController.js'
import { getCwd } from '../../utils/cwd.js'
import { AbortError, errorMessage, isAbortError } from '../../utils/errors.js'
import { lazySchema } from '../../utils/lazySchema.js'
import { jsonStringify } from '../../utils/slowOperations.js'
import {
  appendTaskOutput,
  flushTaskOutput,
} from '../../utils/task/diskOutput.js'
import {
  registerTask,
  updateTaskState,
} from '../../utils/task/framework.js'
import { emitTaskProgress } from '../../utils/task/sdkProgress.js'
import type {
  LocalWorkflowTaskState,
  WorkflowAgentState,
} from '../../tasks/LocalWorkflowTask/LocalWorkflowTask.js'
import {
  LEGACY_WORKFLOW_TOOL_NAME,
  WORKFLOW_MAX_AGENT_CALLS,
  WORKFLOW_TOOL_NAME,
} from './constants.js'
import { findWorkflow } from './registry.js'
import {
  executeWorkflowSource,
  runWorkflowAgent,
  WorkflowAgentIdleTimeoutError,
  type WorkflowAgentMetrics,
  type WorkflowAgentOptions,
} from './runtime.js'

const inputSchema = lazySchema(() =>
  z.strictObject({
    name: z
      .string()
      .min(1)
      .describe('Namespaced workflow name, for example plugin-name:scan'),
    args: z
      .union([
        z.string(),
        z.record(z.string(), z.unknown()),
      ])
      .optional()
      .describe('Workflow arguments as an object or JSON string'),
  }),
)
type InputSchema = ReturnType<typeof inputSchema>

const outputSchema = lazySchema(() =>
  z.object({
    workflow: z.string(),
    task_id: z.string(),
    status: z.enum(['completed', 'incomplete']),
    result: z.unknown(),
    output_file: z.string(),
    duration_ms: z.number(),
  }),
)
type OutputSchema = ReturnType<typeof outputSchema>
type Output = z.infer<OutputSchema>

function cleanLogLine(value: string): string {
  const oneLine = value.replace(/[\r\n\t]+/g, ' ').trim()
  return oneLine.length > 20_000
    ? `${oneLine.slice(0, 20_000)}…`
    : oneLine
}

export function isWorkflowCancellation(error: unknown): boolean {
  return isAbortError(error)
}

export function isWorkflowAgentCancellation(
  error: unknown,
  signal: AbortSignal,
): boolean {
  // The idle watchdog aborts this controller only to stop the underlying
  // generator. That operational timeout remains a failure, never a user
  // cancellation, even though the same signal is necessarily aborted.
  if (
    error instanceof WorkflowAgentIdleTimeoutError ||
    signal.reason instanceof WorkflowAgentIdleTimeoutError
  ) {
    return false
  }
  return isWorkflowCancellation(error) || signal.aborted
}

export function classifyWorkflowResult(result: unknown): {
  outputStatus: 'completed' | 'incomplete'
  taskStatus: 'completed' | 'incomplete'
} {
  const isIncomplete =
    typeof result === 'object' &&
    result !== null &&
    (result as { status?: unknown }).status === 'incomplete'
  return isIncomplete
    ? { outputStatus: 'incomplete', taskStatus: 'incomplete' }
    : { outputStatus: 'completed', taskStatus: 'completed' }
}

export function assertWorkflowInvocationIsRoot(
  toolUseContext: Pick<ToolUseContext, 'agentId'>,
): void {
  if (toolUseContext.agentId !== undefined) {
    throw new Error(
      'Nested agents cannot invoke Workflow: workflow scheduling and cancellation must remain owned by the root agent',
    )
  }
}

export const WorkflowTool = buildTool({
  name: WORKFLOW_TOOL_NAME,
  aliases: [LEGACY_WORKFLOW_TOOL_NAME],
  searchHint: 'run an installed plugin workflow',
  maxResultSizeChars: 200_000,
  shouldDefer: true,
  strict: true,
  get inputSchema(): InputSchema {
    return inputSchema()
  },
  get outputSchema(): OutputSchema {
    return outputSchema()
  },
  async description() {
    return 'Run a JavaScript workflow bundled with CrabCode or shipped by an enabled plugin'
  },
  async prompt() {
    const { workflows, errors } = await import('./registry.js').then(module =>
      module.discoverWorkflows(getCwd()),
    )
    const catalog = workflows
      .map(
        workflow =>
          `- ${workflow.name}: ${workflow.meta.description}${
            workflow.meta.whenToUse
              ? `\n  Use when: ${workflow.meta.whenToUse}`
              : ''
          }`,
      )
      .join('\n')
    const diagnostic =
      errors.length > 0
        ? `\n\n${errors.length} workflow file(s) could not be loaded; invoking a missing workflow returns diagnostics.`
        : ''
    return `Run a workflow by its exact name as listed below (bundled workflows are bare names; plugin workflows are namespaced \`<plugin>:<name>\`). The workflow runtime executes code, structured agent calls, phases, parallel work, and pipelines directly; do not simulate its steps in prose.\n\nAvailable workflows:\n${catalog || '- none'}${diagnostic}`
  },
  isConcurrencySafe() {
    return false
  },
  isReadOnly() {
    // Workflows are generic installed code and may delegate to write-capable
    // agents. Individual nested tool calls remain permission-checked.
    return false
  },
  isOpenWorld() {
    return true
  },
  interruptBehavior() {
    return 'cancel'
  },
  toAutoClassifierInput(input) {
    return { workflow: input.name, args: input.args }
  },
  userFacingName() {
    return 'Workflow'
  },
  renderToolUseMessage(input) {
    return input.name ? `Running workflow ${input.name}` : 'Running workflow'
  },
  renderToolUseProgressMessage(progressMessages) {
    const latest = progressMessages.at(-1)?.data as
      | WorkflowToolProgress
      | undefined
    if (!latest || latest.type !== 'workflow_progress') return null
    return latest.message
  },
  renderToolResultMessage(output) {
    return output.status === 'incomplete'
      ? `Workflow ${output.workflow} returned an incomplete result`
      : `Workflow ${output.workflow} completed`
  },
  mapToolResultToToolResultBlockParam(output, toolUseID) {
    return {
      tool_use_id: toolUseID,
      type: 'tool_result',
      content: jsonStringify(output),
    }
  },
  async call(
    { name, args },
    toolUseContext,
    canUseTool,
    _parentMessage,
    onProgress,
  ) {
    assertWorkflowInvocationIsRoot(toolUseContext)
    const lookup = await findWorkflow(name, getCwd())
    const workflow = lookup.workflow
    if (!workflow) {
      const available =
        lookup.available.length > 0 ? lookup.available.join(', ') : 'none'
      const diagnostics =
        lookup.errors.length > 0
          ? ` Load errors: ${lookup.errors.slice(0, 5).join(' | ')}`
          : ''
      throw new Error(
        `Workflow '${name}' was not found. Available workflows: ${available}.${diagnostics}`,
      )
    }

    // Display copy for this run. `meta` is canonical English (see
    // bundled/deepResearch.ts "Language contract"); the zh-CN overlay is
    // applied here, at the render edge, and nowhere else. In particular the
    // phase-index lookup below still matches against `workflow.meta.phases`
    // — localizing the matching key would make `phase()` miss its declaration
    // in zh-CN and silently fall back to sequential numbering.
    const displayDescription = localizeWorkflowDescription(
      workflow.name,
      workflow.meta.description,
    )
    const displayPhases = workflow.meta.phases.map(item =>
      localizeWorkflowPhase(workflow.name, item.title, item.detail),
    )
    const displayPhaseTitle = (canonical: string): string =>
      localizeWorkflowPhase(workflow.name, canonical).title

    const taskId = generateTaskId('local_workflow')
    const startedAt = Date.now()
    const abortController = createChildAbortController(
      toolUseContext.abortController,
      WORKFLOW_MAX_AGENT_CALLS + 10,
    )
    const rootSetAppState =
      toolUseContext.setAppStateForTasks ?? toolUseContext.setAppState
    const taskState: LocalWorkflowTaskState = {
      ...createTaskStateBase(
        taskId,
        'local_workflow',
        displayDescription,
        toolUseContext.toolUseId,
      ),
      type: 'local_workflow',
      status: 'running',
      workflowName: workflow.name,
      summary: displayDescription,
      phaseIndex: -1,
      phases: displayPhases,
      recentLogs: [],
      agents: {},
      agentsStarted: 0,
      agentsCompleted: 0,
      totalTokens: 0,
      totalToolUses: 0,
      abortController,
    }
    registerTask(taskState, rootSetAppState)

    let logIndex = 0
    let agentIndex = 0
    // Canonical phase title (the `meta.phases[].title` value the script passed
    // to `phase()`), kept apart from its localized label: the former is the
    // matching key, the latter is what the user reads.
    let currentPhase: string | undefined
    let currentPhaseLabel: string | undefined
    let phaseIndex = -1
    let totalTokens = 0
    let totalToolUses = 0
    let agentsStarted = 0
    let agentsCompleted = 0
    const recentLogs: string[] = []

    const emitProgress = (message: string): void => {
      const data: WorkflowToolProgress = {
        type: 'workflow_progress',
        taskId,
        workflow: workflow.name,
        phase: currentPhaseLabel,
        phaseIndex,
        message,
        agentsStarted,
        agentsCompleted,
      }
      onProgress?.({
        toolUseID: `workflow-${taskId}-${logIndex}`,
        data,
      })
      emitTaskProgress({
        taskId,
        toolUseId: toolUseContext.toolUseId,
        description: displayDescription,
        startTime: startedAt,
        totalTokens,
        toolUses: totalToolUses,
        summary: message,
        workflowProgress: [
          {
            type: currentPhase ? 'phase' : 'log',
            index: logIndex,
            phaseIndex: Math.max(phaseIndex, 0),
            title: currentPhaseLabel,
            message,
          },
        ],
      })
    }

    const log = (rawMessage: string): void => {
      const message = cleanLogLine(rawMessage)
      if (!message) return
      logIndex++
      recentLogs.push(message)
      if (recentLogs.length > 100) recentLogs.shift()
      appendTaskOutput(taskId, `[${currentPhaseLabel ?? 'Workflow'}] ${message}\n`)
      updateTaskState<LocalWorkflowTaskState>(
        taskId,
        rootSetAppState,
        task => ({
          ...task,
          recentLogs: [...task.recentLogs.slice(-99), message],
        }),
      )
      emitProgress(message)
    }

    const phase = (titleValue: string): void => {
      const title = cleanLogLine(titleValue)
      if (!title) throw new Error('phase() requires a non-empty title')
      currentPhase = title
      const declaredIndex = workflow.meta.phases.findIndex(
        item => item.title === title,
      )
      phaseIndex =
        declaredIndex >= 0 ? declaredIndex : Math.max(phaseIndex + 1, 0)
      // A title the script invented (no declaration to match) has no catalog
      // entry either, so it stays verbatim — which is what the reader wants:
      // an undeclared phase is the script's own string, not catalog copy.
      currentPhaseLabel = displayPhaseTitle(title)
      updateTaskState<LocalWorkflowTaskState>(
        taskId,
        rootSetAppState,
        task => ({ ...task, currentPhase: currentPhaseLabel, phaseIndex }),
      )
      log(`Phase started: ${currentPhaseLabel}`)
    }

    const updateAgent = (
      agentRunId: string,
      updater: (agent: WorkflowAgentState) => WorkflowAgentState,
    ): void => {
      updateTaskState<LocalWorkflowTaskState>(
        taskId,
        rootSetAppState,
        task => {
          const agent = task.agents[agentRunId]
          if (!agent) return task
          return {
            ...task,
            agents: { ...task.agents, [agentRunId]: updater(agent) },
            agentsStarted,
            agentsCompleted,
            totalTokens,
            totalToolUses,
          }
        },
      )
    }

    const agent = async (
      prompt: string,
      options: WorkflowAgentOptions,
    ): Promise<unknown> => {
      const runNumber = ++agentIndex
      const agentRunId = `wf_${taskId}-${runNumber}`
      const label = cleanLogLine(
        options?.label || options?.agentType || `agent-${runNumber}`,
      )
      const agentType = options?.agentType ?? ''
      const agentPhase = options?.phase ?? currentPhase
      agentsStarted++
      const initial: WorkflowAgentState = {
        id: agentRunId,
        label,
        agentType,
        ...(agentPhase ? { phase: agentPhase } : {}),
        status: 'queued',
      }
      updateTaskState<LocalWorkflowTaskState>(
        taskId,
        rootSetAppState,
        task => ({
          ...task,
          agents: { ...task.agents, [agentRunId]: initial },
          agentsStarted,
        }),
      )
      emitProgress(`${label}: queued`)
      const agentAbortController =
        createChildAbortController(abortController)

      let metrics: WorkflowAgentMetrics = {
        totalTokens: 0,
        totalToolUses: 0,
      }
      try {
        const outcome = await runWorkflowAgent({
          workflow,
          taskId,
          agentRunId,
          prompt,
          options,
          toolUseContext,
          canUseTool,
          abortController: agentAbortController,
          onRunning() {
            updateAgent(agentRunId, entry => ({
              ...entry,
              status: 'running',
              startedAt: Date.now(),
            }))
            emitProgress(`${label}: running`)
          },
        })
        metrics = outcome.metrics
        totalTokens += metrics.totalTokens
        totalToolUses += metrics.totalToolUses
        agentsCompleted++
        updateAgent(agentRunId, entry => ({
          ...entry,
          status: 'completed',
          endedAt: Date.now(),
        }))
        emitProgress(`${label}: completed`)
        return outcome.value
      } catch (error) {
        const cancelled = isWorkflowAgentCancellation(
          error,
          agentAbortController.signal,
        )
        agentsCompleted++
        updateAgent(agentRunId, entry => ({
          ...entry,
          status: cancelled ? 'cancelled' : 'failed',
          endedAt: Date.now(),
          error: errorMessage(error),
        }))
        emitProgress(
          `${label}: ${cancelled ? 'cancelled' : 'failed'} — ${errorMessage(error)}`,
        )
        throw error
      } finally {
        if (!agentAbortController.signal.aborted) {
          agentAbortController.abort(
            new AbortError('Workflow agent lifecycle completed'),
          )
        }
      }
    }

    try {
      log(`Workflow ${workflow.name} started`)
      const result = await executeWorkflowSource({
        workflow,
        args: args ?? {},
        signal: abortController.signal,
        observer: { log, phase },
        agent,
        buildPartialResult: () => ({
          workflow: workflow.name,
          status: 'incomplete',
          reason: `Workflow ${workflow.name} reached its runtime budget before returning a result.`,
          phase: currentPhaseLabel ?? null,
          agents_started: agentsStarted,
          agents_completed: agentsCompleted,
          recent_logs: recentLogs.slice(-20),
        }),
        onRuntimeTerminated(error) {
          if (!abortController.signal.aborted) {
            abortController.abort(error)
          }
        },
      })
      const serializedResult = jsonStringify(result ?? null, null, 2)
      const completion = classifyWorkflowResult(result)
      appendTaskOutput(taskId, `\n[result]\n${serializedResult}\n`)
      await flushTaskOutput(taskId)
      updateTaskState<LocalWorkflowTaskState>(
        taskId,
        rootSetAppState,
        task => ({
          ...task,
          status: completion.taskStatus,
          endTime: Date.now(),
          result,
          agentsStarted,
          agentsCompleted,
          totalTokens,
          totalToolUses,
        }),
      )
      emitProgress(`Workflow ${workflow.name} ${completion.outputStatus}`)
      return {
        data: {
          workflow: workflow.name,
          task_id: taskId,
          status: completion.outputStatus,
          result,
          output_file: taskState.outputFile,
          duration_ms: Date.now() - startedAt,
        },
      }
    } catch (error) {
      // Runtime failures abort the shared controller solely to cancel/drain
      // children. That cleanup signal must not rewrite a real script, budget,
      // watchdog, or agent failure into "Workflow cancelled".
      const aborted = isWorkflowCancellation(error)
      if (!abortController.signal.aborted) {
        abortController.abort(error)
      }
      const message = aborted ? 'Workflow cancelled' : errorMessage(error)
      appendTaskOutput(
        taskId,
        `\n[${aborted ? 'cancelled' : 'failed'}] ${message}\n`,
      )
      await flushTaskOutput(taskId)
      updateTaskState<LocalWorkflowTaskState>(
        taskId,
        rootSetAppState,
        task => ({
          ...task,
          status: aborted ? 'killed' : 'failed',
          endTime: Date.now(),
          error: message,
          agentsStarted,
          agentsCompleted,
          totalTokens,
          totalToolUses,
        }),
      )
      if (aborted) throw new AbortError(message)
      throw error
    }
  },
} satisfies ToolDef<InputSchema, Output, WorkflowToolProgress>)
