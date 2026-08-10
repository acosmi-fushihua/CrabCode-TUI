import { describe, expect, test } from 'bun:test'

import {
  createIdleWatchdog,
  executeWorkflowSource,
  WorkflowAgentIdleTimeoutError,
  WorkflowPartialDrainError,
} from '../../src/tools/WorkflowTool/runtime.js'
import {
  classifyWorkflowResult,
  isWorkflowAgentCancellation,
  WorkflowTool,
} from '../../src/tools/WorkflowTool/WorkflowTool.js'
import {
  parseWorkflowAgentIdleTimeoutMs,
  WORKFLOW_AGENT_IDLE_TIMEOUT_MAX_MS,
  WORKFLOW_AGENT_IDLE_TIMEOUT_MS,
} from '../../src/tools/WorkflowTool/constants.js'
import { parseWorkflowModule } from '../../src/tools/WorkflowTool/registry.js'
import { isTerminalTaskStatus } from '../../src/Task.js'
import { AbortError } from '../../src/utils/errors.js'

function makeWorkflow(body: string) {
  const source = `export const meta = {
  name: 'lifecycle-probe',
  description: 'workflow lifecycle probe',
  phases: [{ title: 'Probe', detail: 'exercise lifecycle' }],
}
${body}
`
  const parsed = parseWorkflowModule(source, 'lifecycle-probe.js')
  return {
    name: 'lifecycle-probe',
    localName: 'lifecycle-probe',
    origin: 'bundled' as const,
    pluginName: 'bundled',
    pluginSource: 'bundled',
    pluginPath: 'bundled',
    filePath: 'bundled:lifecycle-probe',
    source,
    executableSource: parsed.executableSource,
    meta: parsed.meta,
  }
}

const sleep = (milliseconds: number): Promise<void> =>
  new Promise(resolve => setTimeout(resolve, milliseconds))

describe('workflow runtime lifecycle', () => {
  test('rejects idle delays that overflow the runtime timer representation', () => {
    expect(
      parseWorkflowAgentIdleTimeoutMs(
        String(WORKFLOW_AGENT_IDLE_TIMEOUT_MAX_MS),
      ),
    ).toBe(WORKFLOW_AGENT_IDLE_TIMEOUT_MAX_MS)
    expect(
      parseWorkflowAgentIdleTimeoutMs(
        String(WORKFLOW_AGENT_IDLE_TIMEOUT_MAX_MS + 1),
      ),
    ).toBe(WORKFLOW_AGENT_IDLE_TIMEOUT_MS)
    expect(parseWorkflowAgentIdleTimeoutMs(String(Number.MAX_SAFE_INTEGER))).toBe(
      WORKFLOW_AGENT_IDLE_TIMEOUT_MS,
    )
    expect(parseWorkflowAgentIdleTimeoutMs('0')).toBeNull()
  })

  test('returns the host partial result only after recording the live child drain', async () => {
    let started!: () => void
    const childStarted = new Promise<void>(resolve => {
      started = resolve
    })
    const terminations: Error[] = []

    const execution = executeWorkflowSource({
      workflow: makeWorkflow(`void agent('keep running', { agentType: 'research' })
return { script: 'returned-before-child' }`),
      args: {},
      signal: new AbortController().signal,
      observer: { log() {}, phase() {} },
      async agent() {
        started()
        await sleep(250)
        return { completed: true }
      },
      maxRuntimeMs: 120,
      drainGraceMs: 20,
      buildPartialResult: () => ({ status: 'partial', retained: true }),
      onRuntimeTerminated(error) {
        terminations.push(error)
      },
    })

    await childStarted
    await expect(execution).resolves.toEqual({ status: 'partial', retained: true })
    expect(terminations).toHaveLength(1)
    expect(terminations[0]).toBeInstanceOf(WorkflowPartialDrainError)
    expect(terminations[0]).toMatchObject({
      workflowName: 'lifecycle-probe',
      inFlightAgents: 1,
    })
  })

  test('resets on activity and fires once after the renewed idle interval', async () => {
    let calls = 0
    const watchdog = createIdleWatchdog({
      timeoutMs: 25,
      onIdle: () => {
        calls++
      },
    })

    watchdog.arm()
    await sleep(12)
    watchdog.reset()
    await sleep(16)
    expect(calls).toBe(0)
    await sleep(35)
    expect(calls).toBe(1)
    await sleep(35)
    expect(calls).toBe(1)
    watchdog.disarm()
  })

  test('disarming before the interval prevents a late idle failure', async () => {
    let calls = 0
    const watchdog = createIdleWatchdog({
      timeoutMs: 20,
      onIdle: () => {
        calls++
      },
    })

    watchdog.arm()
    watchdog.disarm()
    await sleep(35)
    expect(calls).toBe(0)
  })

  test('idle watchdog abort is a failed agent timeout, not a cancellation', () => {
    const controller = new AbortController()
    const idleError = new WorkflowAgentIdleTimeoutError('research', 25)
    controller.abort(idleError)

    expect(
      isWorkflowAgentCancellation(idleError, controller.signal),
    ).toBe(false)
    expect(
      isWorkflowAgentCancellation(
        new AbortError('user cancelled'),
        new AbortController().signal,
      ),
    ).toBe(true)

    const callerCancelled = new AbortController()
    callerCancelled.abort(new AbortError('user cancelled'))
    expect(
      isWorkflowAgentCancellation(new Error('transport stopped'), callerCancelled.signal),
    ).toBe(true)
  })

  test('partial results remain explicitly incomplete through tool and task schemas', () => {
    const partial = { status: 'incomplete', retained: true }
    expect(classifyWorkflowResult(partial)).toEqual({
      outputStatus: 'incomplete',
      taskStatus: 'incomplete',
    })
    expect(classifyWorkflowResult({ status: 'complete', retained: true })).toEqual({
      outputStatus: 'completed',
      taskStatus: 'completed',
    })
    expect(isTerminalTaskStatus('incomplete')).toBe(true)

    const output = {
      workflow: 'lifecycle-probe',
      task_id: 'w12345678',
      status: 'incomplete',
      result: partial,
      output_file: '/tmp/lifecycle-probe.jsonl',
      duration_ms: 10,
    }
    expect(WorkflowTool.outputSchema.safeParse(output).success).toBe(true)
    expect(
      WorkflowTool.outputSchema.safeParse({ ...output, status: 'running' })
        .success,
    ).toBe(false)
    expect(WorkflowTool.renderToolResultMessage(output)).toContain('incomplete')
  })
})
