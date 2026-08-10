import { afterEach, beforeEach, describe, expect, test } from 'bun:test'
import { selectExposedWorkflows } from '../../src/tools/WorkflowTool/createWorkflowCommand.js'
import {
  discoverWorkflows,
  type DiscoveredWorkflow,
} from '../../src/tools/WorkflowTool/registry.js'
import { resolveWorkflowAgentCandidates } from '../../src/tools/WorkflowTool/runtime.js'
import {
  _resetFeatureCacheForTests,
  _setFeatureOverrideForTests,
} from '../../src/utils/featurePolyfill.js'
import {
  clearBundledWorkflows,
  getBundledWorkflowErrors,
  getBundledWorkflows,
  initBundledWorkflows,
} from '../../src/tools/WorkflowTool/bundled/index.js'
import {
  DEEP_RESEARCH_WORKFLOW_NAME,
  DEEP_RESEARCH_WORKFLOW_SOURCE,
} from '../../src/tools/WorkflowTool/bundled/deepResearch.js'
import { WEB_RESEARCHER_AGENT_TYPE } from '../../src/tools/WorkflowTool/bundled/agents.js'
import { parseWorkflowModule } from '../../src/tools/WorkflowTool/parser.js'

describe('slash command exposure gate', () => {
  const asWorkflow = (
    name: string,
    origin: 'plugin' | 'bundled',
  ): DiscoveredWorkflow => ({
    name,
    localName: name,
    origin,
    pluginName: origin === 'bundled' ? 'crabcode' : 'demo',
    pluginSource: origin === 'bundled' ? 'bundled' : 'demo@local',
    pluginPath: origin === 'bundled' ? 'bundled' : '/plugins/demo',
    filePath: `${name}.js`,
    source: '',
    executableSource: '',
    meta: { name, description: 'd', phases: [] },
  })

  const catalog = [
    asWorkflow('deep-research', 'bundled'),
    asWorkflow('demo:audit', 'plugin'),
  ]

  afterEach(() => {
    _resetFeatureCacheForTests()
  })

  // The narrow gate exists so that shipping a first-party workflow does not
  // also hand every installed plugin an execution entry point.
  test('hides plugin workflows when only the bundled flag is on', () => {
    _setFeatureOverrideForTests('WORKFLOW_SCRIPTS', false)
    expect(selectExposedWorkflows(catalog).map(w => w.name)).toEqual([
      'deep-research',
    ])
  })

  test('exposes everything once the full workflow flag is on', () => {
    _setFeatureOverrideForTests('WORKFLOW_SCRIPTS', true)
    expect(selectExposedWorkflows(catalog).map(w => w.name)).toEqual([
      'deep-research',
      'demo:audit',
    ])
  })
})

describe('bundled deep-research workflow', () => {
  beforeEach(() => {
    clearBundledWorkflows()
  })

  test('registers cleanly with no load errors', () => {
    initBundledWorkflows()
    expect(getBundledWorkflowErrors()).toEqual([])
    const names = getBundledWorkflows().map(workflow => workflow.name)
    expect(names).toEqual([DEEP_RESEARCH_WORKFLOW_NAME])
  })

  test('is un-namespaced and marked bundled', () => {
    initBundledWorkflows()
    const workflow = getBundledWorkflows()[0]!
    // Plugin workflows are always `<plugin>:<name>`; a bare name cannot
    // collide with one.
    expect(workflow.name).toBe(DEEP_RESEARCH_WORKFLOW_NAME)
    expect(workflow.name).not.toContain(':')
    expect(workflow.origin).toBe('bundled')
    expect(workflow.meta.name).toBe(DEEP_RESEARCH_WORKFLOW_NAME)
    expect(workflow.meta.description.length).toBeGreaterThan(0)
    expect(workflow.meta.whenToUse).toBeTruthy()
  })

  test('initialization is idempotent', () => {
    initBundledWorkflows()
    initBundledWorkflows()
    initBundledWorkflows()
    expect(getBundledWorkflows()).toHaveLength(1)
    expect(getBundledWorkflowErrors()).toEqual([])
  })

  describe('script contract', () => {
    const parsed = parseWorkflowModule(
      DEEP_RESEARCH_WORKFLOW_SOURCE,
      '<bundled>/deep-research.js',
    )

    // The script is carried as a `String.raw` template so that backslash
    // escapes inside it (`\n`, `\.`, `\/`) reach the emitted JavaScript
    // verbatim instead of being consumed by TypeScript first. A side effect is
    // that when the transpiler emits non-ASCII as `\uXXXX`, `String.raw`
    // preserves those six characters literally rather than the character.
    //
    // Both forms are correct at runtime — the workflow parser decodes `\u`
    // escapes when reading `meta`, and the vm decodes them when evaluating the
    // body — but source-text assertions have to normalise first, or they would
    // silently depend on which form the current transpiler happens to emit.
    const decodeUnicodeEscapes = (source: string): string =>
      source.replace(/\\u([0-9a-fA-F]{4})/g, (_, hex: string) =>
        String.fromCharCode(Number.parseInt(hex, 16)),
      )
    const body = decodeUnicodeEscapes(parsed.executableSource)

    test('the script body survives transpilation in a runnable form', () => {
      // Whichever form the transpiler picked, no half-decoded escape may
      // remain once normalised. Phase titles are ASCII, so the reachability
      // probe uses a display glyph the body still carries.
      expect(body).toContain('phase("Scope")')
      expect(body).toContain('✓')
      expect(body).not.toContain('\\u')
    })

    // The script is canonical English with a zh-CN overlay applied at the
    // render edge (WORKFLOW_CATALOG_ZH). Writing display copy in Chinese here
    // instead is exactly the drift that made en-US users read Chinese phase
    // names and a Chinese command description: `meta` is baked into the binary
    // as a data literal, so nothing downstream can translate it back.
    test('carries no CJK copy — localization belongs to the catalog overlay', () => {
      // CJK punctuation, unified ideographs, and full-width forms. Built from
      // an escaped pattern string so the gate itself stays pure ASCII and
      // cannot be weakened by an editor rewriting a literal range endpoint.
      const CJK = new RegExp(
        '[\\u3000-\\u303f\\u4e00-\\u9fff\\uff00-\\uffef]',
        'g',
      )
      expect([...body.matchAll(CJK)].map(match => match[0])).toEqual([])
    })

    // The report belongs to the user, so it has to come back in the language
    // they asked in. Pinning that to the question rather than to a UI locale is
    // what keeps an English-canonical script correct for a Chinese user; drop
    // the rule from a prompt and that prompt's output silently turns English.
    test('every prompt pins output language to the question', () => {
      expect(body).toContain('same language as the research question')
      const prompts = [
        'const SEARCH_PROMPT',
        'const FETCH_PROMPT',
        'const VERIFY_PROMPT',
      ]
      for (const marker of prompts) {
        expect(body).toContain(marker)
      }
      // scope / search / fetch / verify / synthesize — every agent prompt.
      const uses = [...body.matchAll(/LANGUAGE_RULE/g)]
      expect(uses.length).toBe(6)
    })

    // The runtime evaluates the body inside a vm as an async function with
    // top-level `return` and `await`. Constructing the same shape here proves
    // the shipped text is syntactically valid JavaScript — a syntax error
    // would otherwise only surface on a user's first invocation, minutes into
    // a run, as an opaque workflow failure.
    test('the script body is syntactically valid as an async function', () => {
      const AsyncFunction = Object.getPrototypeOf(
        async function () {},
      ).constructor as new (...args: string[]) => unknown
      expect(() => {
        new AsyncFunction(
          'args',
          'agent',
          'parallel',
          'pipeline',
          'phase',
          'log',
          'budget',
          'workflow',
          parsed.executableSource,
        )
      }).not.toThrow()
    })

    test('declares five phases', () => {
      expect(parsed.meta.phases.map(phase => phase.title)).toEqual([
        'Scope',
        'Search',
        'Fetch',
        'Verify',
        'Synthesize',
      ])
      for (const phase of parsed.meta.phases) {
        expect(phase.detail).toBeTruthy()
      }
    })

    test('every phase() call matches a declared phase title', () => {
      const called = [
        ...body.matchAll(/\bphase\("([^"]+)"\)/g),
      ].map(match => match[1]!)
      const declared = new Set(parsed.meta.phases.map(phase => phase.title))
      expect(called.length).toBeGreaterThan(0)
      for (const title of called) {
        expect(declared.has(title)).toBe(true)
      }
    })

    test('every agent option object routes through a declared phase', () => {
      const phases = [
        ...body.matchAll(/phase:\s*"([^"]+)"/g),
      ].map(match => match[1]!)
      const declared = new Set(parsed.meta.phases.map(phase => phase.title))
      expect(phases.length).toBeGreaterThan(0)
      for (const title of phases) {
        expect(declared.has(title)).toBe(true)
      }
    })

    // The runtime rejects agent() without options.agentType, and both
    // parallel() and pipeline() are Promise.all — an agent that rejects takes
    // its whole fan-out down with it. Both concerns are handled once in the
    // `ask()` helper, so no call site may reach `agent(` directly.
    test('all agent calls go through the ask() helper', () => {
      const helper = parsed.executableSource.match(
        /const ask = \(prompt, options\) =>\s*\n?\s*agent\(prompt, \{ agentType: AGENT_TYPE, \.\.\.options \}\)\.catch\(/,
      )
      expect(helper).not.toBeNull()

      const directCalls = [
        ...parsed.executableSource.matchAll(/(^|[^.\w])agent\(/g),
      ]
      // Exactly one: the call inside ask() itself.
      expect(directCalls).toHaveLength(1)
    })

    test('pins the agent type to the bundled web researcher', () => {
      expect(parsed.executableSource).toContain(
        `const AGENT_TYPE = '${WEB_RESEARCHER_AGENT_TYPE}'`,
      )
    })

    // Stage 1 of pipeline() feeds its return value straight into stage 2, and
    // stage 1 returns null when a search agent is skipped or fails. Without
    // this guard the null reaches `searchResult.results` and rejects the whole
    // pipeline.
    test('the pipeline second stage guards against a null first stage', () => {
      expect(parsed.executableSource).toContain('if (!searchResult) return []')
    })

    // The soft budget exempts high-relevance sources so a late angle's best
    // result is not lost to an early angle's filler. That exemption has to be
    // bounded by something, or fetch fan-out is decided by how generously the
    // search agents rate their own results.
    test('the high-relevance exemption is bounded by a hard ceiling', () => {
      expect(parsed.executableSource).toContain(
        'if (fetched >= MAX_FETCH_HARD) {',
      )
    })

    // A claim must be positively adjudicated to survive. Counting only
    // refutations would let an all-abstain claim through with refuted === 0.
    test('claim survival requires a quorum of valid votes', () => {
      expect(parsed.executableSource).toContain(
        'const survives = valid.length >= REFUTATIONS_REQUIRED && refuted < REFUTATIONS_REQUIRED',
      )
    })

    test('fan-out stays within the agent concurrency budget', () => {
      const readConstant = (name: string): number => {
        const match = parsed.executableSource.match(
          new RegExp(`const ${name} = (\\d+)`),
        )
        expect(match).not.toBeNull()
        return Number(match![1])
      }
      const votes = readConstant('VOTES_PER_CLAIM')
      const maxFetchHard = readConstant('MAX_FETCH_HARD')
      const maxVerify = readConstant('MAX_VERIFY_CLAIMS')

      expect(votes).toBe(3)
      expect(readConstant('REFUTATIONS_REQUIRED')).toBe(2)
      // The soft budget must not exceed the hard ceiling, or it would never
      // bind and high-relevance sources would be the only thing bounding fetch.
      expect(readConstant('MAX_FETCH')).toBeLessThanOrEqual(maxFetchHard)

      // scope + up to 6 search angles + fetches + votes + synthesis. Fetch is
      // counted at the HARD ceiling, not the soft budget: past MAX_FETCH the
      // script still admits high-relevance sources, so asserting the soft
      // number here would state a bound the code does not enforce.
      const worstCase = 1 + 6 + maxFetchHard + maxVerify * votes + 1
      expect(worstCase).toBeLessThanOrEqual(60)
    })

    test('discloses that the question leaves the machine, and leaves the permission posture to the host', () => {
      expect(body).toContain('web search service')
      // W-WORKFLOW-TURN-BUDGET PR-7: the script used to add "every source
      // fetch asks for approval under your existing permission settings",
      // which is true on none of the paths a workflow actually takes — its
      // agents run with isAsync:true and structurally cannot raise a dialog.
      // Only the host knows whether this surface auto-allows or refuses, so
      // the sentence is injected by
      // `the direct workflow host`. This assertion used to
      // pin the false claim in place; it now pins its absence.
      expect(body).not.toContain('asks for approval')
      expect(body).not.toContain('permission settings')
    })
  })
})

// Both cases below were found by destructive review, not by the happy path.
describe('bundled agent resolution order', () => {
  const activeAgent = {
    agentType: WEB_RESEARCHER_AGENT_TYPE,
    source: 'userSettings',
    baseDir: '/tmp',
    whenToUse: 'impostor',
  } as unknown as Parameters<typeof resolveWorkflowAgentCandidates>[1][number]

  test('a bundled workflow resolves the bundled agent before any same-named user agent', () => {
    const candidates = resolveWorkflowAgentCandidates({ origin: 'bundled' }, [
      activeAgent,
    ])
    const first = candidates.find(
      agent => agent.agentType === WEB_RESEARCHER_AGENT_TYPE,
    )
    // Shadowing would not be a no-op: the ownership check rejects a non
    // built-in agent, so a name collision would hard-fail the workflow.
    expect(first?.source).toBe('built-in')
  })

  test('a plugin workflow never sees the bundled agents', () => {
    const candidates = resolveWorkflowAgentCandidates({ origin: 'plugin' }, [])
    expect(candidates).toEqual([])
  })
})

describe('discovery self-initializes the bundled catalog', () => {
  // Registration must not depend on module evaluation order: `tools.ts`
  // initializes while building the tool list, but the command factory can
  // reach discovery first, and an unregistered catalog would make
  // /deep-research silently vanish rather than fail loudly.
  test('discoverWorkflows registers bundled workflows even when nothing else did', async () => {
    clearBundledWorkflows()
    expect(getBundledWorkflows()).toHaveLength(0)
    const { workflows } = await discoverWorkflows(process.cwd())
    expect(workflows.map(w => w.name)).toContain(DEEP_RESEARCH_WORKFLOW_NAME)
  })
})
