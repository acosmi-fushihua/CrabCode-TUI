import type { SetAppState, Task, TaskStateBase } from '../../Task.js'
import { AbortError } from '../../utils/errors.js'
import { updateTaskState } from '../../utils/task/framework.js'

export type WorkflowAgentState = {
  id: string
  label: string
  agentType: string
  phase?: string
  status:
    | 'queued'
    | 'running'
    | 'completed'
    | 'failed'
    | 'cancelled'
    | 'skipped'
  startedAt?: number
  endedAt?: number
  error?: string
}

export type LocalWorkflowTaskState = TaskStateBase & {
  readonly type: 'local_workflow'
  readonly workflowName: string
  readonly summary?: string
  readonly currentPhase?: string
  readonly phaseIndex: number
  readonly phases: ReadonlyArray<{ title: string; detail?: string }>
  readonly recentLogs: readonly string[]
  readonly agents: Readonly<Record<string, WorkflowAgentState>>
  readonly agentsStarted: number
  readonly agentsCompleted: number
  readonly totalTokens: number
  readonly totalToolUses: number
  readonly result?: unknown
  readonly error?: string
  readonly abortController: AbortController
}

function getWorkflowTask(
  taskId: string,
  setAppState: SetAppState,
  updater: (task: LocalWorkflowTaskState) => LocalWorkflowTaskState,
): void {
  updateTaskState<LocalWorkflowTaskState>(taskId, setAppState, updater)
}

export async function killWorkflowTask(
  taskId: string,
  setAppState?: SetAppState,
): Promise<void> {
  if (!setAppState) return
  let controller: AbortController | undefined
  getWorkflowTask(taskId, setAppState, task => {
    if (task.status !== 'running' && task.status !== 'pending') return task
    controller = task.abortController
    // The WorkflowTool catch path is the single owner of terminal state and
    // output. Killing only signals here, avoiding duplicate status/output
    // writes that can race with the runtime's own cancellation handling.
    return task
  })
  controller?.abort(new AbortError('Workflow cancelled'))
}

export const LocalWorkflowTask: Task = {
  name: 'Workflow',
  type: 'local_workflow',
  async kill(taskId, setAppState) {
    await killWorkflowTask(taskId, setAppState)
  },
}
