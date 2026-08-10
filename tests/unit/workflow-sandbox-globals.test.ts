// Anti-drift sentinel for the workflow sandbox's global surface —
// W-WORKFLOW-TURN-BUDGET PR-5 (2026-08-02).
//
// The bundled `deep-research` workflow shipped with `new URL(...)` wrapped in
// try/catch. It read like defensive programming; it was a branch that always
// fell through, because `vm.createContext` over a null-prototype sandbox hands
// the script the plain ECMAScript intrinsics and no host globals whatsoever.
// De-duplication silently degraded to raw string comparison, five cosmetic
// variants of one page each consumed a fetch from a 10-fetch budget, and every
// source was labelled `fetch:unknown`.
//
// Reading the runtime source cannot catch that class of bug, and no static
// assertion about the script text can either. So the lists in `runtime.ts` are
// measured, and this test re-measures them inside the real sandbox on every
// run: if a future Bun changes vm semantics, that is a red test asking for a
// decision, not another silent degradation.

import { describe, expect, test } from 'bun:test'
import {
  executeWorkflowSource,
  WORKFLOW_SANDBOX_AVAILABLE_GLOBALS,
  WORKFLOW_SANDBOX_MISSING_GLOBALS,
} from '../../src/tools/WorkflowTool/runtime.js'
import { parseWorkflowModule } from '../../src/tools/WorkflowTool/registry.js'

function buildWorkflow(body: string) {
  const source = `export const meta = {
  name: 'probe',
  description: 'sandbox global probe',
  phases: [{ title: 'Probe', detail: 'read globals' }],
}
${body}
`
  const parsed = parseWorkflowModule(source, 'probe.js')
  return {
    name: 'probe',
    localName: 'probe',
    origin: 'bundled' as const,
    pluginName: 'bundled',
    pluginSource: 'bundled',
    pluginPath: 'bundled',
    filePath: 'bundled:probe',
    source,
    executableSource: parsed.executableSource,
    meta: parsed.meta,
  }
}

async function runProbe(body: string): Promise<unknown> {
  return executeWorkflowSource({
    workflow: buildWorkflow(body),
    args: {},
    signal: new AbortController().signal,
    observer: { log() {}, phase() {} },
    async agent() {
      throw new Error('probe workflows must not call agents')
    },
  })
}

describe('workflow sandbox globals', () => {
  test('every global the pinned list claims is available really is', async () => {
    const names = JSON.stringify([...WORKFLOW_SANDBOX_AVAILABLE_GLOBALS])
    const result = (await runProbe(
      `const missing = ${names}.filter(n => typeof globalThis[n] === "undefined")
return { missing }`,
    )) as { missing: string[] }
    expect(result.missing).toEqual([])
  })

  test('every global the pinned list claims is absent really is', async () => {
    const names = JSON.stringify([...WORKFLOW_SANDBOX_MISSING_GLOBALS])
    const result = (await runProbe(
      `const present = ${names}.filter(n => typeof globalThis[n] !== "undefined")
return { present }`,
    )) as { present: string[] }
    expect(result.present).toEqual([])
  })

  test('URL specifically is absent, so string parsing is mandatory', async () => {
    // Called out on its own because this is the exact global whose absence
    // caused the shipped defect, and the generic lists above are easy to edit
    // without noticing what came out of them.
    expect(WORKFLOW_SANDBOX_MISSING_GLOBALS).toContain('URL')
    const result = (await runProbe(
      `let threw = null
try { new URL("https://example.com/") } catch (error) { threw = String(error && error.message) }
return { urlType: typeof globalThis.URL, threw }`,
    )) as { urlType: string; threw: string | null }
    expect(result.urlType).toBe('undefined')
    expect(result.threw).not.toBeNull()
  })

  test('the injected workflow API is present, including the deadline clock', async () => {
    const result = (await runProbe(
      `return {
  api: ["args", "log", "phase", "agent", "pipeline", "parallel", "sleep", "deadline", "setTimeout", "clearTimeout"]
    .filter(n => typeof globalThis[n] === "undefined"),
  remainingIsPositive: deadline.remainingMs() > 0,
  notExceeded: deadline.exceeded(),
}`,
    )) as { api: string[]; remainingIsPositive: boolean; notExceeded: boolean }
    expect(result.api).toEqual([])
    expect(result.remainingIsPositive).toBe(true)
    expect(result.notExceeded).toBe(false)
  })
})
