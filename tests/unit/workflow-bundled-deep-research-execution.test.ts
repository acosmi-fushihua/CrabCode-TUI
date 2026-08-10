// Real execution of the bundled `deep-research` script —
// W-WORKFLOW-TURN-BUDGET PR-9 (2026-08-02).
//
// `workflow-bundled-deep-research.test.ts` is 24 cases of regular expressions
// over the script's source text plus one `new AsyncFunction` syntax check. It
// never calls `executeWorkflowSource`, so it is structurally incapable of
// observing runtime behaviour — and three shipped defects walked straight past
// it (URL de-duplication that never de-duplicated, `fetch:unknown` labels on
// every source, and a script that could not tell it was out of time).
//
// This file runs the actual script through the actual runtime with a stubbed
// `agent()`. Everything asserted here is an observable of the run, not a
// property of the source text.

import { describe, expect, test } from 'bun:test'
import {
  DEEP_RESEARCH_WORKFLOW_NAME,
  DEEP_RESEARCH_WORKFLOW_SOURCE,
} from '../../src/tools/WorkflowTool/bundled/deepResearch.js'
import { parseWorkflowModule } from '../../src/tools/WorkflowTool/registry.js'
import {
  executeWorkflowSource,
  type WorkflowAgentOptions,
} from '../../src/tools/WorkflowTool/runtime.js'

const parsed = parseWorkflowModule(
  DEEP_RESEARCH_WORKFLOW_SOURCE,
  'deep-research.js',
)

const workflow = {
  name: DEEP_RESEARCH_WORKFLOW_NAME,
  localName: DEEP_RESEARCH_WORKFLOW_NAME,
  origin: 'bundled' as const,
  pluginName: 'bundled',
  pluginSource: 'bundled',
  pluginPath: 'bundled',
  filePath: 'bundled:deep-research',
  source: DEEP_RESEARCH_WORKFLOW_SOURCE,
  executableSource: parsed.executableSource,
  meta: parsed.meta,
}

/**
 * Five links to the same arxiv page, differing only in ways that do not change
 * which page they are: `www.`, a trailing slash, an upper-case scheme and
 * host, and a tracking parameter. A working `normURL` collapses all five to
 * one fetch. The pre-fix script issued four.
 */
const SAME_PAGE_VARIANTS = [
  'https://arxiv.org/abs/2401.00001',
  'https://www.arxiv.org/abs/2401.00001',
  'https://arxiv.org/abs/2401.00001/',
  'HTTPS://ARXIV.ORG/abs/2401.00001',
  'https://arxiv.org/abs/2401.00001?utm_source=newsletter',
]

const DISTINCT_PAGES = [
  'https://example.org/a',
  'https://example.org/b',
  'https://other.example.com/c',
  'https://third.example.net/d',
  'https://fourth.example.io/e',
]

const DISTINCT_IPV6_HOSTS = [
  'https://[2001:db8::1]:8443/research',
  'https://[2001:db8::2]:8443/research',
  'https://[2001:db8::3]:8443/research',
  'https://[2001:db8::4]:8443/research',
  'https://[2001:db8::5]:8443/research',
]

type Call = { label: string; phase?: string; prompt: string }

function makeStubAgent(urlsPerAngle: string[], fetchDelayMs = 0) {
  const calls: Call[] = []
  let angleIndex = 0
  const agent = async (
    prompt: string,
    options: WorkflowAgentOptions,
  ): Promise<unknown> => {
    const label = String(options.label ?? '')
    calls.push({ label, phase: options.phase, prompt })
    if (fetchDelayMs > 0 && label.startsWith('fetch:')) {
      await new Promise(resolve => setTimeout(resolve, fetchDelayMs))
    }
    if (label === 'scope') {
      return {
        question: 'q',
        summary: 's',
        angles: Array.from({ length: 5 }, (_, index) => ({
          label: `angle${index}`,
          query: `query ${index}`,
          rationale: `rationale ${index}`,
        })),
      }
    }
    if (label.startsWith('search:')) {
      const url = urlsPerAngle[angleIndex++ % urlsPerAngle.length]
      return {
        results: [
          { url, title: `title for ${url}`, snippet: 'snippet', relevance: 'high' },
        ],
      }
    }
    if (label.startsWith('fetch:')) {
      return {
        sourceQuality: 'primary',
        publishDate: '2026-01-01',
        claims: [
          { claim: `claim-${calls.length}`, quote: 'quote', importance: 'central' },
        ],
      }
    }
    if (/^v\d+:/.test(label)) {
      return { refuted: false, evidence: 'evidence', confidence: 'high' }
    }
    if (label === 'synthesize') {
      return { summary: 'S', findings: [], caveats: 'C', openQuestions: [] }
    }
    throw new Error(`unexpected agent label: ${label}`)
  }
  return { agent, calls }
}

async function run(
  urlsPerAngle: string[],
  overrides: { maxRuntimeMs?: number; fetchDelayMs?: number } = {},
) {
  const { fetchDelayMs, ...runtimeOverrides } = overrides
  const { agent, calls } = makeStubAgent(urlsPerAngle, fetchDelayMs ?? 0)
  const logs: string[] = []
  const phases: string[] = []
  const result = (await executeWorkflowSource({
    workflow,
    args: 'recursive self-improvement papers',
    signal: new AbortController().signal,
    observer: {
      log(message) {
        logs.push(message)
      },
      phase(title) {
        phases.push(title)
      },
    },
    agent,
    ...runtimeOverrides,
  })) as Record<string, unknown>
  return { result, calls, logs, phases }
}

function labelsWithPrefix(calls: Call[], prefix: string): string[] {
  return calls.filter(call => call.label.startsWith(prefix)).map(c => c.label)
}

describe('deep-research: real execution', () => {
  test('cosmetic URL variants of one page collapse to a single fetch', async () => {
    const { result, calls } = await run(SAME_PAGE_VARIANTS)
    // The decisive assertion: `URL` does not exist in the sandbox, so this
    // only passes with a string-based normalizer.
    expect(labelsWithPrefix(calls, 'fetch:')).toHaveLength(1)
    const stats = result.stats as Record<string, number>
    expect(stats.sourcesFetched).toBe(1)
    expect(stats.urlDupes).toBe(4)
  })

  test('fetch labels carry the real host instead of "unknown"', async () => {
    const { calls } = await run(SAME_PAGE_VARIANTS)
    expect(labelsWithPrefix(calls, 'fetch:')).toEqual(['fetch:arxiv.org'])
  })

  test('distinct pages are all fetched and each labelled by its own host', async () => {
    const { calls, result } = await run(DISTINCT_PAGES)
    expect(labelsWithPrefix(calls, 'fetch:').sort()).toEqual([
      'fetch:example.org',
      'fetch:example.org',
      'fetch:other.example.com',
      'fetch:third.example.net',
      'fetch:fourth.example.io',
    ].sort())
    const stats = result.stats as Record<string, number>
    expect(stats.sourcesFetched).toBe(5)
    expect(stats.urlDupes).toBe(0)
  })

  test('bracketed IPv6 hosts stay distinct when paths and ports match', async () => {
    const { calls, result } = await run(DISTINCT_IPV6_HOSTS)
    expect(labelsWithPrefix(calls, 'fetch:').sort()).toEqual(
      DISTINCT_IPV6_HOSTS.map(url => {
        const bracketEnd = url.indexOf(']')
        return `fetch:${url.slice(url.indexOf('['), bracketEnd + 1)}`
      }).sort(),
    )
    const stats = result.stats as Record<string, number>
    expect(stats.sourcesFetched).toBe(5)
    expect(stats.urlDupes).toBe(0)
  })

  test('phases run in the declared order and the agent count matches the fan-out formula', async () => {
    const { calls, phases, result } = await run(DISTINCT_PAGES)
    expect(phases).toEqual(['Scope', 'Search', 'Fetch', 'Verify', 'Synthesize'])
    const stats = result.stats as Record<string, number>
    // 1 scope + 5 search + N fetch + 3 votes per verified claim + 1 synthesis.
    const expected =
      1 + 5 + stats.sourcesFetched + stats.claimsVerified * 3 + 1
    expect(calls).toHaveLength(expected)
    expect(stats.agentCalls).toBe(expected)
  })

  test('a budget exhausted mid-run hands back the fetched sources instead of losing them', async () => {
    // Every search and fetch is dispatched immediately, so all five fetches
    // start well inside the 150ms budget; each then takes 400ms, guaranteeing
    // the budget is gone by the time the script reaches its pre-verification
    // check. Wide margin in both directions, so the ordering is not a race.
    const { result, calls } = await run(DISTINCT_PAGES, {
      maxRuntimeMs: 150,
      fetchDelayMs: 400,
    })
    expect(result.status).toBe('incomplete')
    expect(result.unverifiedClaims).toBeArray()
    expect((result.unverifiedClaims as unknown[]).length).toBeGreaterThan(0)
    // Verification is the expensive stage; entering it with no budget left is
    // exactly what the guard exists to prevent.
    expect(labelsWithPrefix(calls, 'v0:')).toHaveLength(0)
    expect(labelsWithPrefix(calls, 'fetch:').length).toBeGreaterThan(0)
  })

  test('a budget exhausted before scoping says so instead of blaming the agent', async () => {
    const { result } = await run(DISTINCT_PAGES, { maxRuntimeMs: 1 })
    expect(String(result.error)).toContain('time budget')
  })

  test('the run no longer claims that sources are approved one at a time', async () => {
    // The host now states the real posture (the direct workflow host);
    // the script must not re-assert a permission outcome it cannot observe.
    const { logs } = await run(DISTINCT_PAGES)
    expect(logs.join('\n')).not.toContain('asks for approval')
  })
})
