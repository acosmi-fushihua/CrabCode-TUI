import { type FSWatcher, watch } from 'node:fs'
import { logForDebugging } from './debug.js'
import {
  claimTask,
  ensureTasksDir,
  getTasksDir,
  listTasks,
  type Task,
  updateTask,
} from './tasks.js'

const DEFAULT_DEBOUNCE_MS = 1000

export type TaskListWatcherCoreOptions = {
  taskListId: string
  /** The task-list ID is also the established tasks-mode claimant identity. */
  agentId?: string
  isBusy: () => boolean
  /**
   * Submit through the active surface's ordinary incoming-user-message path.
   * False (or a thrown error) means the claim must be released.
   */
  onSubmitTask: (prompt: string) => boolean
  debounceMs?: number
}

/**
 * Transport-neutral tasks-mode controller.
 *
 * Task storage, locking and claim authority remain in tasks.ts. This class
 * only owns lifecycle and scheduling so React and the direct StructuredIO
 * runtime cannot drift into separate task-selection implementations.
 */
export class TaskListWatcherCore {
  private readonly taskListId: string
  private readonly agentId: string
  private readonly isBusy: () => boolean
  private readonly onSubmitTask: (prompt: string) => boolean
  private readonly debounceMs: number
  private currentTaskId: string | null = null
  private watcher: FSWatcher | null = null
  private debounceTimer: ReturnType<typeof setTimeout> | null = null
  private checkChain: Promise<void> = Promise.resolve()
  private started = false
  private stopped = false

  constructor(options: TaskListWatcherCoreOptions) {
    this.taskListId = options.taskListId
    this.agentId = options.agentId ?? options.taskListId
    this.isBusy = options.isBusy
    this.onSubmitTask = options.onSubmitTask
    this.debounceMs = options.debounceMs ?? DEFAULT_DEBOUNCE_MS
  }

  /**
   * Install the filesystem watcher. checkNow() remains independently callable
   * for deterministic hosts/tests and for lifecycle-triggered idle checks.
   */
  async start(): Promise<void> {
    if (this.started || this.stopped) return
    this.started = true
    await ensureTasksDir(this.taskListId)
    if (this.stopped) return

    const tasksDir = getTasksDir(this.taskListId)
    try {
      this.watcher = watch(tasksDir, () => this.scheduleCheck())
      this.watcher.unref()
      logForDebugging(`[TaskListWatcher] Watching for tasks in ${tasksDir}`)
    } catch (error) {
      logForDebugging(
        `[TaskListWatcher] Failed to watch ${tasksDir}: ${error}`,
      )
    }
    this.scheduleCheck()
  }

  /** Schedule a debounced check when the host transitions to idle. */
  notifyIdle(): void {
    if (this.stopped) return
    this.scheduleCheck()
  }

  /**
   * Run a serialized task check immediately.
   *
   * Serialization is required because an fs event and an idle transition can
   * race. Without it both checks can observe the same unowned task before the
   * first claim finishes and submit it twice.
   */
  checkNow(): Promise<void> {
    const check = this.checkChain.then(
      () => this.checkForTasks(),
      () => this.checkForTasks(),
    )
    this.checkChain = check.catch(error => {
      logForDebugging(`[TaskListWatcher] Task check failed: ${error}`)
    })
    return this.checkChain
  }

  stop(): void {
    if (this.stopped) return
    this.stopped = true
    this.watcher?.close()
    this.watcher = null
    if (this.debounceTimer) {
      clearTimeout(this.debounceTimer)
      this.debounceTimer = null
    }
  }

  private scheduleCheck(): void {
    if (this.stopped) return
    if (this.debounceTimer) clearTimeout(this.debounceTimer)
    this.debounceTimer = setTimeout(() => {
      this.debounceTimer = null
      void this.checkNow()
    }, this.debounceMs)
    this.debounceTimer.unref?.()
  }

  private async checkForTasks(): Promise<void> {
    if (this.stopped || this.isBusy()) return

    const tasks = await listTasks(this.taskListId)
    if (this.stopped || this.isBusy()) return

    if (this.currentTaskId !== null) {
      const currentTask = tasks.find(task => task.id === this.currentTaskId)
      if (!currentTask || currentTask.status === 'completed') {
        logForDebugging(
          `[TaskListWatcher] Task #${this.currentTaskId} is marked complete, ready for next task`,
        )
        this.currentTaskId = null
      } else {
        return
      }
    }

    const availableTask = findAvailableTask(tasks)
    if (!availableTask) return

    logForDebugging(
      `[TaskListWatcher] Found available task #${availableTask.id}: ${availableTask.subject}`,
    )
    const result = await claimTask(
      this.taskListId,
      availableTask.id,
      this.agentId,
    )
    if (!result.success) {
      logForDebugging(
        `[TaskListWatcher] Failed to claim task #${availableTask.id}: ${result.reason}`,
      )
      return
    }

    // The host can become busy (or shut down) while the filesystem lock is
    // being acquired. Never dispatch into a stopped surface or jump ahead of
    // user input; release the just-acquired authoritative claim instead.
    if (this.stopped || this.isBusy()) {
      await updateTask(this.taskListId, availableTask.id, {
        owner: undefined,
      })
      return
    }

    this.currentTaskId = availableTask.id
    const prompt = formatTaskAsPrompt(availableTask)
    logForDebugging(
      `[TaskListWatcher] Submitting task #${availableTask.id} as prompt`,
    )

    let submitted = false
    try {
      submitted = this.onSubmitTask(prompt)
    } catch (error) {
      logForDebugging(
        `[TaskListWatcher] Task #${availableTask.id} submit threw: ${error}`,
      )
    }
    if (submitted) return

    logForDebugging(
      `[TaskListWatcher] Failed to submit task #${availableTask.id}, releasing claim`,
    )
    await updateTask(this.taskListId, availableTask.id, {
      owner: undefined,
    })
    this.currentTaskId = null
  }
}

/**
 * Find the first pending, unowned task whose dependencies are all complete.
 */
export function findAvailableTask(tasks: Task[]): Task | undefined {
  const unresolvedTaskIds = new Set(
    tasks.filter(task => task.status !== 'completed').map(task => task.id),
  )
  return tasks.find(task => {
    if (task.status !== 'pending') return false
    if (task.owner) return false
    return task.blockedBy.every(id => !unresolvedTaskIds.has(id))
  })
}

export function formatTaskAsPrompt(task: Task): string {
  let prompt = `Complete all open tasks. Start with task #${task.id}: \n\n ${task.subject}`
  if (task.description) prompt += `\n\n${task.description}`
  return prompt
}
