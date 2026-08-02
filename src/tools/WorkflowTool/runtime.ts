import type { CanUseToolFn } from '../../hooks/useCanUseTool.js'
import { Worker } from 'node:worker_threads'
import { getTotalCost } from '../../cost-tracker.js'
import {
  type AgentSlotTicket,
  BackgroundSlotCancelledError,
  getBackgroundAgentScheduler,
} from '../../services/agents/BackgroundAgentScheduler.js'
import type { ToolPermissionContext, ToolUseContext } from '../../Tool.js'
import type { Message } from '../../types/message.js'
import { createChildAbortController } from '../../utils/abortController.js'
import { getQuerySourceForAgent } from '../../utils/promptCategory.js'
import { AbortError, errorMessage, isAbortError } from '../../utils/errors.js'
import {
  createUserMessage,
  extractTextContent,
  getLastAssistantMessage,
} from '../../utils/messages.js'
import {
  filterDeniedAgents,
} from '../../utils/permissions/permissions.js'
import { getTokenCountFromUsage } from '../../utils/tokens.js'
import {
  countToolUses,
} from '../AgentTool/agentToolUtils.js'
import { AGENT_TOOL_NAME } from '../AgentTool/constants.js'
import {
  isBuiltInAgent,
  type AgentDefinition,
} from '../AgentTool/loadAgentsDir.js'
import { runAgent } from '../AgentTool/runAgent.js'
import {
  createSyntheticOutputTool,
  SYNTHETIC_OUTPUT_TOOL_NAME,
} from '../SyntheticOutputTool/SyntheticOutputTool.js'
import {
  WORKFLOW_AGENT_DRAIN_TIMEOUT_MS,
  WORKFLOW_HEARTBEAT_TIMEOUT_MS,
  WORKFLOW_MAX_AGENT_PROMPT_BYTES,
  WORKFLOW_MAX_AGENT_CALLS,
  WORKFLOW_MAX_AGENT_SCHEMA_BYTES,
  WORKFLOW_MAX_ARGS_BYTES,
  WORKFLOW_MAX_RESULT_BYTES,
  WORKFLOW_MAX_RUNTIME_MS,
  WORKFLOW_MAX_STEPS,
  WORKFLOW_WORKER_RESOURCE_LIMITS,
} from './constants.js'
import { getBundledWorkflowAgents } from './bundled/agents.js'
import type { DiscoveredWorkflow } from './registry.js'

export type WorkflowAgentOptions = {
  agentType: string
  label?: string
  phase?: string
  schema?: Record<string, unknown>
  effort?: string | number
  model?: string
}

function jsonByteLength(
  value: unknown,
  label: string,
  maximum: number,
): number {
  let serialized: string
  try {
    serialized = JSON.stringify(value)
  } catch {
    throw new Error(`Workflow ${label} must be JSON-serializable`)
  }
  if (serialized === undefined) {
    throw new Error(`Workflow ${label} must be JSON-serializable`)
  }
  const bytes = Buffer.byteLength(serialized, 'utf8')
  if (bytes > maximum) {
    throw new Error(
      `Workflow ${label} is ${bytes} bytes; maximum is ${maximum}`,
    )
  }
  return bytes
}

export type WorkflowAgentMetrics = {
  totalTokens: number
  totalToolUses: number
}

export type WorkflowRuntimeObserver = {
  log(message: string): void
  phase(title: string): void
  agentStarted(event: {
    id: string
    label: string
    agentType: string
    phase?: string
  }): void
  agentFinished(event: {
    id: string
    label: string
    agentType: string
    phase?: string
    status: 'completed' | 'failed'
    error?: string
    metrics: WorkflowAgentMetrics
  }): void
}

export type WorkflowAgentRunner = (
  prompt: string,
  options: WorkflowAgentOptions,
) => Promise<unknown>

export type WorkflowBudgetLease = {
  release(): void
}

type WorkflowBudgetWaiter = {
  signal: AbortSignal
  resolve: (lease: WorkflowBudgetLease) => void
  reject: (error: Error) => void
  onAbort: () => void
}

/**
 * Process-wide single-flight gate for agents governed by maxBudgetUsd.
 *
 * The underlying cost counter is process-global and is only authoritative
 * after a model response reports usage. Serializing budgeted workflow roots
 * ensures that the unavoidable overshoot is bounded to one in-flight response,
 * rather than one response per workflow fan-out branch. Nested foreground
 * agents inherit the same modelBudgetGuard and execute under their root lease.
 */
export class WorkflowBudgetGate {
  private held = false
  private readonly queue: WorkflowBudgetWaiter[] = []

  acquire(signal: AbortSignal): Promise<WorkflowBudgetLease> {
    if (signal.aborted) {
      return Promise.reject(new AbortError('Workflow cancelled'))
    }
    if (!this.held) {
      this.held = true
      return Promise.resolve(this.createLease())
    }
    return new Promise<WorkflowBudgetLease>((resolve, reject) => {
      const waiter: WorkflowBudgetWaiter = {
        signal,
        resolve,
        reject,
        onAbort: () => {
          const index = this.queue.indexOf(waiter)
          if (index < 0) return
          this.queue.splice(index, 1)
          signal.removeEventListener('abort', waiter.onAbort)
          reject(new AbortError('Workflow cancelled'))
        },
      }
      this.queue.push(waiter)
      signal.addEventListener('abort', waiter.onAbort, { once: true })
    })
  }

  private createLease(): WorkflowBudgetLease {
    let released = false
    return {
      release: () => {
        if (released) return
        released = true
        this.promote()
      },
    }
  }

  private promote(): void {
    while (this.queue.length > 0) {
      const waiter = this.queue.shift()!
      waiter.signal.removeEventListener('abort', waiter.onAbort)
      if (waiter.signal.aborted) {
        waiter.reject(new AbortError('Workflow cancelled'))
        continue
      }
      waiter.resolve(this.createLease())
      return
    }
    this.held = false
  }
}

export class WorkflowBudgetExceededError extends Error {
  override readonly name = 'WorkflowBudgetExceededError'
}

export function createWorkflowModelBudgetGuard(
  maxBudgetUsd: number,
  readTotalCost: () => number = getTotalCost,
): () => Error | undefined {
  if (!Number.isFinite(maxBudgetUsd) || maxBudgetUsd < 0) {
    throw new Error('Workflow maxBudgetUsd must be a finite non-negative number')
  }
  return () => {
    const observedCost = readTotalCost()
    if (!Number.isFinite(observedCost)) {
      return new WorkflowBudgetExceededError(
        'Workflow budget cannot be enforced because observed cost is not finite',
      )
    }
    if (observedCost < maxBudgetUsd) return undefined
    return new WorkflowBudgetExceededError(
      `Workflow budget exhausted ($${maxBudgetUsd}; observed $${observedCost})`,
    )
  }
}

const workflowBudgetGate = new WorkflowBudgetGate()

type WorkerMessage =
  | { type: 'ready' | 'heartbeat' }
  | { type: 'log' | 'phase'; value: string }
  | {
      type: 'agent'
      id: number
      prompt: string
      options: WorkflowAgentOptions
    }
  | { type: 'result'; value: unknown }
  | { type: 'error'; message: string }

const WORKFLOW_WORKER_SOURCE = String.raw`
'use strict'
const { parentPort, workerData } = require('node:worker_threads')
const vm = require('node:vm')

const hostTimers = new Map()
let deliverToContext
let completed = false

function fail(message) {
  if (completed) return
  completed = true
  clearInterval(heartbeat)
  for (const timer of hostTimers.values()) clearTimeout(timer)
  hostTimers.clear()
  parentPort.postMessage({ type: 'error', message: String(message) })
  parentPort.close()
}

function finish(value) {
  if (completed) return
  completed = true
  clearInterval(heartbeat)
  for (const timer of hostTimers.values()) clearTimeout(timer)
  hostTimers.clear()
  parentPort.postMessage({ type: 'result', value })
  parentPort.close()
}

function deliver(message) {
  if (completed || !deliverToContext) return
  try {
    deliverToContext(JSON.stringify(message))
  } catch (error) {
    fail(error && error.message ? error.message : error)
  }
}

function hostBridge(rawMessage) {
  if (completed) return
  let message
  try {
    message = JSON.parse(String(rawMessage))
  } catch {
    fail('Workflow runtime bridge received invalid JSON')
    return
  }
  if (!message || typeof message !== 'object') {
    fail('Workflow runtime bridge received an invalid message')
    return
  }
  if (message.type === 'log' || message.type === 'phase') {
    parentPort.postMessage({
      type: message.type,
      value: String(message.value),
    })
    return
  }
  if (message.type === 'agent') {
    parentPort.postMessage({
      type: 'agent',
      id: message.id,
      prompt: message.prompt,
      options: message.options,
    })
    return
  }
  if (message.type === 'timer-set') {
    const id = message.id
    const delay = message.delay
    if (
      !Number.isInteger(id) ||
      !Number.isFinite(delay) ||
      delay < 0 ||
      delay > workerData.maxRuntimeMs
    ) {
      fail('Workflow runtime received an invalid timer')
      return
    }
    const timer = setTimeout(() => {
      hostTimers.delete(id)
      deliver({ type: 'timer', id })
    }, delay)
    hostTimers.set(id, timer)
    return
  }
  if (message.type === 'timer-clear') {
    const timer = hostTimers.get(message.id)
    if (timer) {
      clearTimeout(timer)
      hostTimers.delete(message.id)
    }
    return
  }
  if (message.type === 'result') {
    finish(message.value)
    return
  }
  if (message.type === 'error') {
    fail(message.message || 'Workflow failed')
    return
  }
  fail('Workflow runtime bridge received an unknown message')
}

function registerDelivery(callback) {
  if (deliverToContext || typeof callback !== 'function') {
    throw new Error('Workflow runtime delivery channel was registered twice')
  }
  deliverToContext = callback
}

const heartbeat = setInterval(() => {
  parentPort.postMessage({ type: 'heartbeat' })
}, 250)

try {
  const sandbox = Object.create(null)
  Object.assign(sandbox, {
    __crabcodeHostBridge: hostBridge,
    __crabcodeRegisterDelivery: registerDelivery,
    __crabcodeArgsJson: workerData.argsJson,
    __crabcodeMaxSteps: workerData.maxSteps,
    __crabcodeMaxAgentCalls: workerData.maxAgentCalls,
    __crabcodeMaxRuntimeMs: workerData.maxRuntimeMs,
  })
  const context = vm.createContext(sandbox, {
    name: 'crabcode-workflow',
    codeGeneration: { strings: false, wasm: false },
  })
  const bootstrap = new vm.Script(
    String.raw${'`'}
'use strict'
;(() => {
  const bridge = globalThis.__crabcodeHostBridge
  const registerDelivery = globalThis.__crabcodeRegisterDelivery
  const argsJson = globalThis.__crabcodeArgsJson
  const maxSteps = globalThis.__crabcodeMaxSteps
  const maxAgentCalls = globalThis.__crabcodeMaxAgentCalls
  const maxRuntimeMs = globalThis.__crabcodeMaxRuntimeMs
  delete globalThis.__crabcodeHostBridge
  delete globalThis.__crabcodeRegisterDelivery
  delete globalThis.__crabcodeArgsJson
  delete globalThis.__crabcodeMaxSteps
  delete globalThis.__crabcodeMaxAgentCalls
  delete globalThis.__crabcodeMaxRuntimeMs

  const safeParse = JSON.parse.bind(JSON)
  const safeStringify = JSON.stringify.bind(JSON)
  const safeThen = Function.call.bind(Promise.prototype.then)
  const pendingAgents = new Map()
  const timerCallbacks = new Map()
  let nextAgentId = 0
  let nextTimerId = 0
  let agentCalls = 0
  let terminal = false

  function errorMessage(error) {
    try {
      return error && error.message ? String(error.message) : String(error)
    } catch {
      return 'Workflow failed'
    }
  }

  function send(message) {
    bridge(safeStringify(message))
  }

  function terminate(type, value) {
    if (terminal) return
    terminal = true
    try {
      send(type === 'result'
        ? { type: 'result', value }
        : { type: 'error', message: errorMessage(value) })
    } catch (error) {
      try {
        bridge(safeStringify({
          type: 'error',
          message: 'Workflow result is not JSON-serializable: ' + errorMessage(error),
        }))
      } catch {
        // The host watchdog is the final fail-closed path.
      }
    }
  }

  function assertCount(kind, count) {
    if (!Number.isInteger(count) || count < 0 || count > maxSteps) {
      throw new Error(kind + ' received ' + count + ' items; maximum is ' + maxSteps)
    }
  }

  function workflowSetTimeout(handler, milliseconds, ...handlerArgs) {
    if (typeof handler !== 'function') {
      throw new Error('Workflow setTimeout accepts a function only')
    }
    const delay = milliseconds === undefined ? 0 : Number(milliseconds)
    if (!Number.isFinite(delay) || delay < 0 || delay > maxRuntimeMs) {
      throw new Error('Workflow setTimeout requires a bounded non-negative delay')
    }
    const id = ++nextTimerId
    timerCallbacks.set(id, { handler, handlerArgs })
    send({ type: 'timer-set', id, delay })
    return id
  }

  function workflowClearTimeout(id) {
    timerCallbacks.delete(id)
    send({ type: 'timer-clear', id })
  }

  function sleep(milliseconds) {
    return new Promise(resolve => workflowSetTimeout(resolve, milliseconds))
  }

  function log(value) {
    send({ type: 'log', value: String(value) })
  }

  function phase(value) {
    send({ type: 'phase', value: String(value) })
  }

  function agent(prompt, options) {
    agentCalls += 1
    if (agentCalls > maxAgentCalls) {
      return Promise.reject(
        new Error('workflow exceeded the maximum of ' + maxAgentCalls + ' agent calls'),
      )
    }
    const id = ++nextAgentId
    return new Promise((resolve, reject) => {
      pendingAgents.set(id, { resolve, reject })
      try {
        send({
          type: 'agent',
          id,
          prompt: String(prompt),
          options,
        })
      } catch (error) {
        pendingAgents.delete(id)
        reject(error)
      }
    })
  }

  async function parallel(jobs) {
    if (!Array.isArray(jobs) || jobs.some(job => typeof job !== 'function')) {
      throw new Error('parallel() requires an array of functions')
    }
    assertCount('parallel()', jobs.length)
    return Promise.all(jobs.map(job => job()))
  }

  async function pipeline(items, map, next) {
    if (!Array.isArray(items) || typeof map !== 'function') {
      throw new Error('pipeline() requires an array and a mapping function')
    }
    if (next !== undefined && typeof next !== 'function') {
      throw new Error('pipeline() next stage must be a function')
    }
    assertCount('pipeline()', items.length)
    return Promise.all(
      items.map(async (item, index) => {
        const mapped = await map(item, index)
        return next ? next(mapped, index) : mapped
      }),
    )
  }

  registerDelivery(rawMessage => {
    if (terminal) return
    let message
    try {
      message = safeParse(rawMessage)
    } catch {
      terminate('error', new Error('Workflow delivery channel received invalid JSON'))
      return
    }
    if (message.type === 'agent-result') {
      const pending = pendingAgents.get(message.id)
      if (!pending) return
      pendingAgents.delete(message.id)
      if (message.ok) pending.resolve(message.value)
      else pending.reject(new Error(String(message.error || 'Workflow agent failed')))
      return
    }
    if (message.type === 'timer') {
      const timer = timerCallbacks.get(message.id)
      if (!timer) return
      timerCallbacks.delete(message.id)
      try {
        timer.handler(...timer.handlerArgs)
      } catch (error) {
        terminate('error', error)
      }
    }
  })

  Object.assign(globalThis, {
    args: safeParse(argsJson),
    log,
    phase,
    agent,
    pipeline,
    parallel,
    sleep,
    setTimeout: workflowSetTimeout,
    clearTimeout: workflowClearTimeout,
  })

  Object.defineProperty(globalThis, '__crabcodeRunWorkflow', {
    configurable: true,
    value(run) {
      delete globalThis.__crabcodeRunWorkflow
      let result
      try {
        result = run()
      } catch (error) {
        terminate('error', error)
        return
      }
      safeThen(result, value => terminate('result', value), error => terminate('error', error))
    },
  })
})()
${'`'},
    { filename: 'crabcode-workflow-bootstrap.js' },
  )
  bootstrap.runInContext(context, {
    timeout: 2_000,
    breakOnSigint: false,
  })
  const source =
    '__crabcodeRunWorkflow(async () => { "use strict";\n' +
    workerData.executableSource +
    '\n})'
  const script = new vm.Script(source, {
    filename: workerData.filename,
    importModuleDynamically() {
      throw new Error('Dynamic import is not available in Workflow scripts')
    },
  })
  parentPort.postMessage({ type: 'ready' })
  script.runInContext(context, {
    timeout: 2_000,
    breakOnSigint: false,
  })
} catch (error) {
  fail(error && error.message ? error.message : error)
}

parentPort.on('message', message => {
  if (!message || message.type !== 'agent-result') return
  try {
    deliver({
      type: 'agent-result',
      id: message.id,
      ok: message.ok,
      ...(message.ok ? { value: message.value } : { error: message.error }),
    })
  } catch (error) {
    fail(error && error.message ? error.message : error)
  }
})
`

function throwIfAborted(signal: AbortSignal): void {
  if (signal.aborted) {
    const reason = signal.reason
    if (isAbortError(reason)) throw reason
    throw new AbortError(
      reason instanceof Error ? reason.message : 'Workflow cancelled',
    )
  }
}

export async function executeWorkflowSource(params: {
  workflow: DiscoveredWorkflow
  args: unknown
  signal: AbortSignal
  observer: Pick<WorkflowRuntimeObserver, 'log' | 'phase'>
  agent: WorkflowAgentRunner
  onRuntimeTerminated?: (error: Error) => void
}): Promise<unknown> {
  const {
    workflow,
    args,
    signal,
    observer,
    agent,
    onRuntimeTerminated,
  } = params
  throwIfAborted(signal)
  jsonByteLength(args, 'arguments', WORKFLOW_MAX_ARGS_BYTES)
  const argsJson = JSON.stringify(args)
  if (argsJson === undefined) {
    throw new Error('Workflow arguments must be JSON-serializable')
  }
  return new Promise<unknown>((resolve, reject) => {
    let settled = false
    let lastHeartbeat = Date.now()
    let workerResultReceived = false
    let workerResult: unknown
    const activeAgentCalls = new Set<Promise<void>>()
    const worker = new Worker(WORKFLOW_WORKER_SOURCE, {
      eval: true,
      resourceLimits: WORKFLOW_WORKER_RESOURCE_LIMITS,
      workerData: {
        argsJson,
        executableSource: workflow.executableSource,
        filename: `crabcode-workflow-${workflow.name}.js`,
        maxSteps: WORKFLOW_MAX_STEPS,
        maxAgentCalls: WORKFLOW_MAX_AGENT_CALLS,
        maxRuntimeMs: WORKFLOW_MAX_RUNTIME_MS,
      },
    })

    const cleanup = (): void => {
      signal.removeEventListener('abort', onAbort)
      globalThis.clearTimeout(deadline)
      globalThis.clearInterval(watchdog)
      worker.removeAllListeners()
    }
    const settle = (
      outcome: { ok: true; value: unknown } | { ok: false; error: Error },
    ): void => {
      if (settled) return
      settled = true
      cleanup()
      void worker.terminate()
      if (outcome.ok) {
        resolve(outcome.value)
        return
      }
      try {
        onRuntimeTerminated?.(outcome.error)
      } catch {
        // Runtime termination must still drain and reject even if the host's
        // cancellation observer fails.
      }
      const active = [...activeAgentCalls]
      if (active.length === 0) {
        reject(outcome.error)
        return
      }
      let drainTimer: ReturnType<typeof globalThis.setTimeout> | undefined
      const drainTimeout = new Promise<void>(finish => {
        drainTimer = globalThis.setTimeout(
          finish,
          WORKFLOW_AGENT_DRAIN_TIMEOUT_MS,
        )
      })
      void Promise.race([
        Promise.allSettled(active).then(() => undefined),
        drainTimeout,
      ]).finally(() => {
        if (drainTimer !== undefined) globalThis.clearTimeout(drainTimer)
        reject(outcome.error)
      })
    }
    const finishWorkerResultIfDrained = (): void => {
      if (
        settled ||
        !workerResultReceived ||
        activeAgentCalls.size !== 0
      ) {
        return
      }
      try {
        jsonByteLength(
          workerResult,
          'result',
          WORKFLOW_MAX_RESULT_BYTES,
        )
        settle({ ok: true, value: workerResult })
      } catch (error) {
        settle({
          ok: false,
          error:
            error instanceof Error
              ? error
              : new Error(errorMessage(error)),
        })
      }
    }
    const onAbort = (): void => {
      settle({ ok: false, error: new AbortError('Workflow cancelled') })
    }
    signal.addEventListener('abort', onAbort, { once: true })

    const deadline = globalThis.setTimeout(() => {
      settle({
        ok: false,
        error: new Error(
          `Workflow ${workflow.name} exceeded its ${WORKFLOW_MAX_RUNTIME_MS}ms runtime limit`,
        ),
      })
    }, WORKFLOW_MAX_RUNTIME_MS)
    const watchdog = globalThis.setInterval(() => {
      if (Date.now() - lastHeartbeat <= WORKFLOW_HEARTBEAT_TIMEOUT_MS) return
      settle({
        ok: false,
        error: new Error(
          `Workflow ${workflow.name} stopped responding and was terminated`,
        ),
      })
    }, Math.max(250, Math.floor(WORKFLOW_HEARTBEAT_TIMEOUT_MS / 3)))

    worker.on('message', (raw: WorkerMessage) => {
      if (settled || !raw || typeof raw !== 'object') return
      if (raw.type === 'heartbeat' || raw.type === 'ready') {
        lastHeartbeat = Date.now()
        return
      }
      if (raw.type === 'log') {
        observer.log(raw.value)
        return
      }
      if (raw.type === 'phase') {
        observer.phase(raw.value)
        return
      }
      if (raw.type === 'result') {
        // A workflow is not complete while any agent RPC it launched is
        // still alive. This catches `void agent(...); return result` instead
        // of reporting success and orphaning the child model run.
        workerResultReceived = true
        workerResult = raw.value
        finishWorkerResultIfDrained()
        return
      }
      if (raw.type === 'error') {
        settle({
          ok: false,
          error: new Error(
            `Workflow ${workflow.name} failed: ${raw.message}`,
          ),
        })
        return
      }
      if (raw.type === 'agent') {
        let activeCall!: Promise<void>
        activeCall = Promise.resolve()
          .then(() => {
            validateAgentOptions(raw.prompt, raw.options)
            return agent(raw.prompt, raw.options)
          })
          .then(
          value => {
            if (!settled && !workerResultReceived) {
              worker.postMessage({
                type: 'agent-result',
                id: raw.id,
                ok: true,
                value,
              })
            }
          },
          error => {
            if (settled) return
            if (error instanceof WorkflowBudgetExceededError) {
              // Budget exhaustion is a host policy boundary, not an exception
              // plugin-authored workflow code may catch and suppress.
              settle({ ok: false, error })
              return
            }
            if (workerResultReceived) {
              settle({
                ok: false,
                error: new Error(
                  `Workflow ${workflow.name} left a rejected agent call unawaited: ${errorMessage(error)}`,
                ),
              })
            } else {
              worker.postMessage({
                type: 'agent-result',
                id: raw.id,
                ok: false,
                error: errorMessage(error),
              })
            }
          },
          )
          .finally(() => {
            activeAgentCalls.delete(activeCall)
            finishWorkerResultIfDrained()
          })
        activeAgentCalls.add(activeCall)
      }
    })
    worker.on('error', error => {
      settle({
        ok: false,
        error: error instanceof Error ? error : new Error(errorMessage(error)),
      })
    })
    worker.on('exit', code => {
      if (!settled) {
        settle({
          ok: false,
          error: new Error(
            `Workflow ${workflow.name} worker exited unexpectedly (${code})`,
          ),
        })
      }
    })
  })
}

function validateAgentOptions(
  prompt: string,
  options: WorkflowAgentOptions,
): void {
  if (typeof prompt !== 'string' || !prompt.trim()) {
    throw new Error('workflow agent() requires a non-empty prompt')
  }
  if (Buffer.byteLength(prompt, 'utf8') > WORKFLOW_MAX_AGENT_PROMPT_BYTES) {
    throw new Error(
      `workflow agent() prompt exceeds ${WORKFLOW_MAX_AGENT_PROMPT_BYTES} bytes`,
    )
  }
  if (
    !options ||
    typeof options !== 'object' ||
    typeof options.agentType !== 'string' ||
    !options.agentType.trim()
  ) {
    throw new Error('workflow agent() requires options.agentType')
  }
  if (
    options.schema !== undefined &&
    (options.schema === null ||
      typeof options.schema !== 'object' ||
      Array.isArray(options.schema))
  ) {
    throw new Error('workflow agent() schema must be a JSON Schema object')
  }
  if (options.schema !== undefined) {
    jsonByteLength(
      options.schema,
      'agent schema',
      WORKFLOW_MAX_AGENT_SCHEMA_BYTES,
    )
  }
  if (options.model !== undefined || options.effort !== undefined) {
    throw new Error(
      'workflow agent() cannot override model or effort; the installed agent definition is authoritative',
    )
  }
  for (const [name, value] of [
    ['label', options.label],
    ['phase', options.phase],
  ] as const) {
    if (
      value !== undefined &&
      (typeof value !== 'string' ||
        Buffer.byteLength(value, 'utf8') > 1_000)
    ) {
      throw new Error(`workflow agent() ${name} must be at most 1000 bytes`)
    }
  }
}

export function isAgentOwnedByWorkflow(
  workflow: Pick<DiscoveredWorkflow, 'origin' | 'pluginSource'>,
  agent: {
    source: AgentDefinition['source']
    plugin?: string
  },
): boolean {
  if (workflow.origin === 'bundled') {
    // A bundled workflow ships inside the executable, so the agents at its own
    // trust level are the built-in ones — not whatever plugins happen to be
    // installed. Widening this to plugin agents would let first-party code be
    // steered by third-party definitions; narrowing it to a hardcoded agent
    // list would put a second, silently-drifting allowlist next to the
    // workflow's own `agentType` strings.
    return agent.source === 'built-in'
  }
  // `pluginSource` is the installed-instance identity used by
  // loadPluginAgents (`AgentDefinition.plugin`). Comparing display namespace
  // names would conflate two sources that declare the same manifest name.
  return agent.source === 'plugin' && agent.plugin === workflow.pluginSource
}

/**
 * Agent definitions a workflow may resolve `agentType` against.
 *
 * Bundled workflows additionally see the workflow-only agents that ship in the
 * binary. Those are deliberately absent from `activeAgents` so they never
 * reach the model-facing catalog, and they are placed **first** so a user- or
 * plugin-defined agent sharing the name cannot shadow the one the bundled
 * workflow was written against: resolution order decides which definition
 * `find` returns, and the ownership check would then reject the impostor,
 * turning a name collision into a hard failure of a first-party workflow.
 *
 * Deny rules are applied by the caller *after* this merge, so a user who
 * denies `Agent(web-researcher)` still removes it.
 */
export function resolveWorkflowAgentCandidates(
  workflow: Pick<DiscoveredWorkflow, 'origin'>,
  activeAgents: AgentDefinition[],
): AgentDefinition[] {
  if (workflow.origin !== 'bundled') return activeAgents
  return [...getBundledWorkflowAgents(), ...activeAgents]
}

function cloneAgentForWorkflow(
  selectedAgent: AgentDefinition,
  options: WorkflowAgentOptions,
  hasStructuredOutput: boolean,
): AgentDefinition {
  const tools =
    hasStructuredOutput && selectedAgent.tools !== undefined
      ? [...new Set([...selectedAgent.tools, SYNTHETIC_OUTPUT_TOOL_NAME])]
      : selectedAgent.tools
  return {
    ...selectedAgent,
    ...(tools !== undefined ? { tools } : {}),
  }
}

export function constrainWorkflowAgentPermissions(
  agent: AgentDefinition,
  parent: ToolPermissionContext,
): {
  agent: AgentDefinition
  workerPermissionContext: ToolPermissionContext
  allowedTools: string[]
} {
  return {
    agent: {
      ...agent,
      // Plugin frontmatter is capability description, not an authority source.
      // A child may narrow the caller's mode but cannot safely widen it, and
      // today's mode lattice has no general "meet" operation. Pinning to the
      // caller is therefore the only unambiguous non-escalating choice.
      permissionMode: parent.mode,
    },
    workerPermissionContext: {
      ...parent,
      // assembleToolPool should see the same non-escalating session view that
      // runAgent receives below. Permanent deny/ask rules remain untouched.
      alwaysAllowRules: {
        ...(parent.alwaysAllowRules.cliArg
          ? { cliArg: [...parent.alwaysAllowRules.cliArg] }
          : {}),
        session: [],
      },
      mode: parent.mode,
    },
    // runAgent interprets an explicit empty list as "clear every inherited
    // allow source except process-start CLI grants".
    allowedTools: [],
  }
}

export async function runWorkflowAgent(params: {
  workflow: DiscoveredWorkflow
  taskId: string
  agentRunId: string
  prompt: string
  options: WorkflowAgentOptions
  toolUseContext: ToolUseContext
  canUseTool: CanUseToolFn
  abortController: AbortController
  onRunning?: () => void
}): Promise<{
  value: unknown
  metrics: WorkflowAgentMetrics
}> {
  const {
    workflow,
    taskId,
    agentRunId,
    prompt,
    options,
    toolUseContext,
    canUseTool,
    abortController,
    onRunning,
  } = params
  validateAgentOptions(prompt, options)
  const appState = toolUseContext.getAppState()
  const allowedByParent =
    toolUseContext.options.agentDefinitions.allowedAgentTypes
  if (
    allowedByParent?.length &&
    !allowedByParent.includes(options.agentType)
  ) {
    throw new Error(
      `Workflow agent '${options.agentType}' is outside the caller's Agent(...) allowlist`,
    )
  }
  // Bundled workflows may additionally resolve the workflow-only agents that
  // ship in the binary. Those are deliberately absent from `activeAgents` so
  // they never reach the model-facing catalog; appending them here — before
  // `filterDeniedAgents` rather than after — keeps user deny rules such as
  // `Agent(web-researcher)` authoritative over them too.
  const candidateAgents = resolveWorkflowAgentCandidates(
    workflow,
    toolUseContext.options.agentDefinitions.activeAgents,
  )
  const visibleAgents = filterDeniedAgents(
    candidateAgents,
    appState.toolPermissionContext,
    AGENT_TOOL_NAME,
  )
  const selectedAgent = visibleAgents.find(
    agentDefinition => agentDefinition.agentType === options.agentType,
  )
  if (!selectedAgent) {
    throw new Error(`Workflow agent type not found: ${options.agentType}`)
  }
  if (!isAgentOwnedByWorkflow(workflow, selectedAgent)) {
    throw new Error(
      workflow.origin === 'bundled'
        ? `Workflow ${workflow.name} is bundled and may call built-in agents only`
        : `Workflow ${workflow.name} may call agents from its exact installed plugin source only`,
    )
  }

  let syntheticTool
  if (options.schema) {
    const synthetic = createSyntheticOutputTool(options.schema)
    if ('error' in synthetic) {
      throw new Error(`Invalid workflow agent schema: ${synthetic.error}`)
    }
    syntheticTool = synthetic.tool
  }

  const permissionPolicy = constrainWorkflowAgentPermissions(
    cloneAgentForWorkflow(
      selectedAgent,
      options,
      Boolean(syntheticTool),
    ),
    appState.toolPermissionContext,
  )
  const workflowAgent = permissionPolicy.agent
  // Dynamic import avoids the tools.ts → WorkflowTool → tools.ts module cycle.
  const { assembleToolPool } = await import('../../tools.js')
  const baseTools = assembleToolPool(
    permissionPolicy.workerPermissionContext,
    appState.mcp.tools,
  )
  const availableTools = syntheticTool
    ? [...baseTools, syntheticTool]
    : baseTools
  const maxBudgetUsd = toolUseContext.options.maxBudgetUsd
  const modelBudgetGuard =
    maxBudgetUsd === undefined
      ? undefined
      : createWorkflowModelBudgetGuard(maxBudgetUsd)
  const sealedToolUseContext: ToolUseContext = {
    ...toolUseContext,
    requireCanUseTool: true,
    suppressUntrustedHooks: true,
    ...(modelBudgetGuard ? { modelBudgetGuard } : {}),
  }

  const budgetLease =
    maxBudgetUsd === undefined
      ? undefined
      : await workflowBudgetGate.acquire(abortController.signal)
  let slotTicket: AgentSlotTicket | undefined
  const cancelQueued = () => {
    slotTicket?.cancelIfQueued()
  }
  try {
    throwIfAborted(abortController.signal)
    slotTicket = getBackgroundAgentScheduler().acquire(agentRunId)
    abortController.signal.addEventListener('abort', cancelQueued, {
      once: true,
    })
    try {
      await slotTicket.whenAvailable
    } catch (error) {
      if (
        error instanceof BackgroundSlotCancelledError ||
        abortController.signal.aborted
      ) {
        throw new AbortError('Workflow cancelled')
      }
      throw error
    }
    throwIfAborted(abortController.signal)
    const initialBudgetError = modelBudgetGuard?.()
    if (initialBudgetError) throw initialBudgetError
    onRunning?.()

    const messages: Message[] = []
    const structuredOutputs: unknown[] = []
    for await (const message of runAgent({
      agentDefinition: workflowAgent,
      promptMessages: [createUserMessage({ content: prompt })],
      toolUseContext: {
        ...sealedToolUseContext,
        // This marker is created only after the root ticket is running.
        // Descendant foreground AgentTools borrow this same lease instead of
        // deadlocking on a second slot from the process-global pool.
        agentSchedulerRootLeaseHeld: true,
      },
      canUseTool,
      isAsync: true,
      querySource: getQuerySourceForAgent(
        workflowAgent.agentType,
        isBuiltInAgent(workflowAgent),
      ),
      override: { abortController },
      availableTools,
      // Parent session approvals are ambient mutable state, not authority that
      // a plugin-authored Workflow may silently inherit. Preserve only
      // process-start CLI grants (runAgent's documented contract) and require
      // normal permission evaluation for every other operation.
      allowedTools: permissionPolicy.allowedTools,
      transcriptSubdir: `workflows/${taskId}`,
      description: options.label ?? workflowAgent.agentType,
      requireFinalText: !syntheticTool,
    })) {
      messages.push(message)
      if (
        message.type === 'attachment' &&
        message.attachment.type === 'structured_output'
      ) {
        structuredOutputs.push(message.attachment.data)
      }
    }
    throwIfAborted(abortController.signal)

    const lastAssistant = getLastAssistantMessage(messages)
    const metrics: WorkflowAgentMetrics = {
      totalTokens: messages.reduce(
        (sum, message) =>
          message.type === 'assistant'
            ? sum + getTokenCountFromUsage(message.message.usage)
            : sum,
        0,
      ),
      totalToolUses: countToolUses(messages),
    }
    if (syntheticTool) {
      if (structuredOutputs.length === 0) {
        throw new Error(
          `Workflow agent ${options.agentType} returned no structured output`,
        )
      }
      if (structuredOutputs.length !== 1) {
        throw new Error(
          `Workflow agent ${options.agentType} returned ${structuredOutputs.length} structured outputs; expected exactly one`,
        )
      }
      jsonByteLength(
        structuredOutputs[0],
        'agent result',
        WORKFLOW_MAX_RESULT_BYTES,
      )
      return { value: structuredOutputs[0], metrics }
    }
    const textResult = lastAssistant
      ? extractTextContent(lastAssistant.message.content, '\n')
      : null
    if (
      textResult !== null &&
      Buffer.byteLength(textResult, 'utf8') > WORKFLOW_MAX_RESULT_BYTES
    ) {
      throw new Error(
        `Workflow agent ${options.agentType} result exceeds ${WORKFLOW_MAX_RESULT_BYTES} bytes`,
      )
    }
    return {
      value: textResult,
      metrics,
    }
  } finally {
    abortController.signal.removeEventListener('abort', cancelQueued)
    slotTicket?.release()
    budgetLease?.release()
  }
}
