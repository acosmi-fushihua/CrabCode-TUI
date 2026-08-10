/**
 * The bundled `deep-research` workflow.
 *
 * The script is held as source text rather than as a module because the
 * workflow runtime executes it inside a `vm` sandbox in a worker thread; it is
 * never imported into this process. `String.raw` is used so backslash escapes
 * inside the script (`\n`, regex classes) survive verbatim into the emitted
 * JavaScript instead of being interpreted by TypeScript first.
 *
 * Ported from the upstream harness with five behavioural corrections that the
 * CrabCode runtime forces. Each is load-bearing, not cosmetic:
 *
 * 1. `agent()` requires `options.agentType`; the upstream calls omit it and
 *    would throw on the first agent.
 * 2. `parallel()` and `pipeline()` are `Promise.all` — fail-fast, not
 *    null-tolerant. The upstream relies on a rejected agent resolving to
 *    `null` ("treat as abstain"); here one rejected verifier vote would reject
 *    its `parallel`, reject the outer `parallel`, and destroy a run that is
 *    already many minutes and many agents deep. Every agent call therefore
 *    catches to `null` itself, which restores the abstain semantics the vote
 *    counting was written against.
 * 3. `pipeline` passes stage 1's return value straight into stage 2, so a
 *    `null` from a skipped search agent reaches `searchResult.results` and
 *    throws. Stage 2 guards its input.
 * 4. Fan-out is retuned for the process-wide agent concurrency budget
 *    (`BackgroundAgentScheduler`, default 3). See MAX_FETCH / MAX_VERIFY_CLAIMS.
 * 5. Every display string in the script is English, and the prompts carry an
 *    explicit "answer in the language of the question" rule. See the language
 *    contract below.
 *
 * ## Language contract
 *
 * Three different kinds of string live in this file and they are localized in
 * three different ways. Getting them confused is what makes a workflow show
 * Chinese copy to an English user (or the reverse):
 *
 * - **`meta` (description / whenToUse / phase titles and details)** is catalog
 *   copy: it renders in the slash-command menu and the workflow detail dialog,
 *   next to every other command's description. It follows the repository-wide
 *   convention — canonical English here, zh-CN overlay in
 *   `src/i18n/catalogLocalization.ts::WORKFLOW_CATALOG_ZH`, applied at display
 *   time. `catalogLocalization-coverage.test.ts` fails if an entry is missing,
 *   so a new bundled workflow cannot silently ship English-only.
 * - **`log()` lines** are program output, like a build log. They stay English
 *   in every locale, matching every other tool in the repository (`Running
 *   workflow ...`, `Starting ...`).
 * - **Prompts** decide what language the *research report* comes back in. They
 *   are English so the instructions are unambiguous to the model, and every
 *   one of them ends with LANGUAGE_RULE, which pins the natural-language
 *   output to whatever language the user asked the question in. That is
 *   locale-independent by construction: no table to maintain, and it stays
 *   correct for a user who asks in a third language.
 *
 * Phase titles are simultaneously a display string and a key — `phase("Scope")`
 * is matched against `meta.phases[].title` to resolve the phase index — so the
 * canonical English title is what the script uses, and localization happens
 * strictly at the render edge (`WorkflowTool.ts`), never in the matching path.
 */

export const DEEP_RESEARCH_WORKFLOW_NAME = 'deep-research'

export const DEEP_RESEARCH_WORKFLOW_SOURCE = String.raw`export const meta = {
  name: 'deep-research',
  description: 'Deep research workflow: parallel multi-angle search, source fetching, adversarial claim verification, and a cited research report.',
  whenToUse: 'Use when the user needs an in-depth, multi-source, fact-checked research report on a topic. Judge first whether the question is specific enough: if it is too broad (for example "which car should I buy" with no budget, use case or region), ask 2-3 clarifying questions to narrow it down and pass the refined question as args. This workflow runs dozens of sub-agents and usually takes more than ten minutes, so it is not for questions that can simply be answered directly.',
  phases: [
    { title: 'Scope', detail: 'Break the research question into 5 complementary search angles' },
    { title: 'Search', detail: 'One parallel search agent per angle' },
    { title: 'Fetch', detail: 'De-duplicate URLs, then fetch sources and extract falsifiable claims' },
    { title: 'Verify', detail: 'Three adversarial votes per claim (2 refutations eliminate it)' },
    { title: 'Synthesize', detail: 'Merge semantic duplicates, rank by confidence, attach citations' },
  ],
}

// deep-research: Scope -> pipeline(Search -> URL dedupe -> Fetch+extract) -> 3-vote Verify -> Synthesize

const AGENT_TYPE = 'web-researcher'
const VOTES_PER_CLAIM = 3
const REFUTATIONS_REQUIRED = 2

// Fan-out budget. The process-wide agent scheduler admits 3 concurrent agents
// by default (env CRABCODE_MAX_CONCURRENT_AGENTS raises it), and
// everything past that queues rather than failing. Verification dominates the
// bill at VOTES_PER_CLAIM agents per claim, so it is the lever that decides
// wall-clock. At these values a full run is roughly 1 + 5 + 10 + 36 + 1 = 53
// agents. Raising MAX_VERIFY_CLAIMS to the upstream 25 would put 75 of ~92
// agents in verification alone and roughly double the run for claims that are
// already ranked below the top dozen by importance and source quality.
const MAX_FETCH = 10
// Hard ceiling. MAX_FETCH alone is a *soft* budget: past it only medium/low
// relevance sources are dropped, so that a late angle's high-signal result is
// not lost to an early angle's filler. Without a second ceiling that exemption
// is unbounded — five angles returning six "high" results each would issue 30
// fetches against a 3-wide scheduler.
const MAX_FETCH_HARD = 15
const MAX_VERIFY_CLAIMS = 12

// Appended to every prompt. The workflow's own copy is English, but the
// research it produces belongs to the user, so the report has to come back in
// the language they asked in. Pinning it to the question rather than to a UI
// locale keeps this correct for a user whose interface language and question
// language differ.
const LANGUAGE_RULE = "\n\nWrite every natural-language field of your output in the same language as the research question above."

// --- Schemas ---
const SCOPE_SCHEMA = {
  type: "object", required: ["question", "angles", "summary"],
  properties: {
    question: { type: "string" },
    summary: { type: "string" },
    angles: { type: "array", minItems: 3, maxItems: 6, items: {
      type: "object", required: ["label", "query"],
      properties: {
        label: { type: "string" },
        query: { type: "string" },
        rationale: { type: "string" },
      },
    }},
  },
}
const SEARCH_SCHEMA = {
  type: "object", required: ["results"],
  properties: {
    results: { type: "array", maxItems: 6, items: {
      type: "object", required: ["url", "title", "relevance"],
      properties: {
        url: { type: "string" },
        title: { type: "string" },
        snippet: { type: "string" },
        relevance: { enum: ["high", "medium", "low"] },
      },
    }},
  },
}
const EXTRACT_SCHEMA = {
  type: "object", required: ["claims", "sourceQuality"],
  properties: {
    sourceQuality: { enum: ["primary", "secondary", "blog", "forum", "unreliable"] },
    publishDate: { type: "string" },
    claims: { type: "array", maxItems: 5, items: {
      type: "object", required: ["claim", "quote", "importance"],
      properties: {
        claim: { type: "string" },
        quote: { type: "string" },
        importance: { enum: ["central", "supporting", "tangential"] },
      },
    }},
  },
}
const VERDICT_SCHEMA = {
  type: "object", required: ["refuted", "evidence", "confidence"],
  properties: {
    refuted: { type: "boolean" },
    evidence: { type: "string" },
    confidence: { enum: ["high", "medium", "low"] },
    counterSource: { type: "string" },
  },
}
const REPORT_SCHEMA = {
  type: "object", required: ["summary", "findings", "caveats"],
  properties: {
    summary: { type: "string" },
    findings: { type: "array", items: {
      type: "object", required: ["claim", "confidence", "sources", "evidence"],
      properties: {
        claim: { type: "string" },
        confidence: { enum: ["high", "medium", "low"] },
        sources: { type: "array", items: { type: "string" } },
        evidence: { type: "string" },
        vote: { type: "string" },
      },
    }},
    caveats: { type: "string" },
    openQuestions: { type: "array", items: { type: "string" } },
  },
}

// Every agent call funnels through here. The runtime's parallel()/pipeline()
// are Promise.all: an agent that rejects would take its whole fan-out with it,
// so a failure is converted into the same null the user-skip path produces and
// the callers' existing null handling deals with both uniformly.
// Agent calls that produced nothing usable, by phase. Every null below is a
// piece of evidence the report will not contain, and a report that does not say
// so overstates its own coverage — the 2026-08-03 audit found a claim passing
// "2-0 (1 abstained)" with nothing anywhere recording that a third of its
// adjudication had been lost.
//
// NOTE: this whole script is a template literal in the enclosing .ts file, so
// neither a backtick nor a dollar-brace may appear anywhere in it — including
// inside comments. A backtick ends the literal; a dollar-brace starts an
// interpolation. Both fail at compile time, which is the good outcome.
const degraded = { scope: 0, search: 0, fetch: 0, verify: 0, synth: 0, other: 0 }
const ask = (prompt, options) =>
  agent(prompt, { agentType: AGENT_TYPE, ...options }).catch(error => {
    const bucket = (options.phase || "other").toLowerCase()
    if (degraded[bucket] === undefined) degraded.other++
    else degraded[bucket]++
    log("Agent failed (" + (options.label || AGENT_TYPE) + "): " + ((error && error.message) || error))
    return null
  })
/** Total agent calls that yielded nothing. */
const degradedTotal = () =>
  degraded.scope + degraded.search + degraded.fetch + degraded.verify + degraded.synth + degraded.other

// --- Phase 0: Scope ---
phase("Scope")
const QUESTION = (typeof args === "string" && args.trim()) || ""
if (!QUESTION) {
  return { error: "No research question was provided. Pass one as args: Workflow({name: 'deep-research', args: '<your research question>'})." }
}
// The permission posture of this run is announced by the host before the
// script starts (WorkflowTool.ts::describePermissionPosture) — only the host
// can see whether this surface auto-allows or refuses what it cannot ask
// about, and the sentence the script used to print here was true on no path a
// workflow actually takes. Do not restate the old wording even in a comment:
// the drift gate greps this whole file, so quoting the retired phrase would
// keep it passing forever.
log("This run sends the question and its search queries to the configured web search service.")
log("Question: " + QUESTION.slice(0, 80) + (QUESTION.length > 80 ? "…" : ""))

const scope = await ask(
  "Break the following research question into complementary search angles.\n\n" +
  "## Research question\n" + QUESTION + "\n\n" +
  "## Task\n" +
  "Produce 5 distinct web search queries that together cover the question from different directions. Fit the angles to the question's domain, for example:\n" +
  "- General: broad/authoritative · academic/technical · recent news · contrarian/critical · practical/applied\n" +
  "- Medical: anatomy and physiology · common causes · serious conditions to rule out · clinical guidelines · red flags\n" +
  "- Technical: state of the art · benchmarks · limitations · industry adoption · cost and trade-offs\n\n" +
  "Make each query specific enough to hit high-signal results, and do not let them overlap. Write each search query in whichever language is most likely to surface good sources.\n" +
  "Return: the question (lightly normalised if needed), a one or two sentence note on how you decomposed it, and the angles." +
  LANGUAGE_RULE + "\n\nReturn structured output only.",
  { label: "scope", schema: SCOPE_SCHEMA }
)
if (!scope) {
  // Distinguish "the agent failed" from "there was no time left to run it".
  // The host refuses new agents once the runtime budget is gone, which arrives
  // here as the same null and would otherwise be reported as an agent fault.
  return {
    error: deadline.exceeded()
      ? "The run reached its time budget before the research question could be decomposed, so nothing was searched."
      : "The scoping agent returned nothing, so the research question could not be decomposed.",
  }
}
log("Scoped into " + scope.angles.length + " angles: " + scope.angles.map(a => a.label).join(", "))

// --- Dedupe state: accumulates as each search agent finishes ---
// URL parsing is done with string operations, not the URL constructor: the
// workflow sandbox is a bare vm context that provides the ECMAScript
// intrinsics and no host globals at all, so URL is undefined here (see
// WORKFLOW_SANDBOX_MISSING_GLOBALS in runtime.ts). The original code wrapped
// new URL in try/catch, which read like defensive programming but was a
// branch that always fell through to raw lowercasing — five cosmetic variants
// of one page counted as five distinct sources and each consumed a fetch.
const stripURLScheme = u => String(u).replace(/^[a-zA-Z][a-zA-Z0-9+.-]*:\/\//, "")
const authorityHost = authority => {
  const hostPort = String(authority).split("@").pop()
  // A bracketed IPv6 literal contains colons that are part of the address,
  // not a host/port separator. Preserve the complete bracketed host and drop
  // only a suffix after the closing bracket (normally the port).
  if (hostPort.startsWith("[")) {
    const close = hostPort.indexOf("]")
    return close === -1 ? hostPort : hostPort.slice(0, close + 1)
  }
  return hostPort.split(":")[0]
}
const urlHost = u => {
  const hostAndPath = stripURLScheme(u)
  const end = hostAndPath.search(/[/?#]/)
  const authority = end === -1 ? hostAndPath : hostAndPath.slice(0, end)
  // Drop userinfo and port; keep the registrable host only.
  const host = authorityHost(authority)
  return host.replace(/^www\./i, "").toLowerCase()
}
const normURL = u => {
  const hostAndPath = stripURLScheme(u)
  // Query strings and fragments are tracking/navigation noise for dedupe
  // purposes; two links differing only by ?utm_source= are the same page.
  const withoutQuery = hostAndPath.split("#")[0].split("?")[0]
  const slash = withoutQuery.indexOf("/")
  const authority = slash === -1 ? withoutQuery : withoutQuery.slice(0, slash)
  const path = slash === -1 ? "" : withoutQuery.slice(slash)
  const host = authorityHost(authority).replace(/^www\./i, "")
  return (host + path.replace(/\/+$/, "")).toLowerCase()
}
const seen = new Map()
const dupes = []
const budgetDropped = []
const relRank = { high: 0, medium: 1, low: 2 }
let fetchSlots = MAX_FETCH
let fetched = 0

// --- Prompts ---
const SEARCH_PROMPT = (angle) =>
  "## Search agent: " + angle.label + "\n\n" +
  "Research question: " + QUESTION + "\n\n" +
  "Your angle: **" + angle.label + "** — " + (angle.rationale || "") + "\n" +
  "Suggested query: " + angle.query + "\n\n" +
  "## Task\nUse WebSearch with the query above (or your own improved version) and return the 4-6 most relevant results.\n" +
  "Rank by relevance to the **original research question**, not by how well a result matches the query string. Skip obvious SEO spam and content farms.\n" +
  "Add one sentence per result explaining why it is relevant." +
  LANGUAGE_RULE + "\n\nReturn structured output only."

const FETCH_PROMPT = (source, angle) =>
  "## Source extraction agent\n\n" +
  "Research question: " + QUESTION + "\n\n" +
  "Fetch this source and extract its key claims:\n" +
  "**URL:** " + source.url + "\n**Title:** " + source.title + "\n**From angle:** " + angle + "\n\n" +
  "## Task\n1. Use WebFetch to retrieve the page.\n" +
  "2. Judge the source quality: primary research or institutional original? secondary reporting? blog or opinion? forum? unreliable?\n" +
  "3. Extract 2-5 **falsifiable** claims relevant to the research question. Each claim must:\n" +
  "   - be a specific, checkable statement (no vague generalities)\n" +
  "   - carry a direct quote from the source that supports it\n" +
  "   - be marked central/supporting/tangential relative to the research question\n" +
  "4. Record the publication date if the page states one.\n\n" +
  "If the fetch fails, the page is paywalled, or it is irrelevant, return claims: [] and sourceQuality: \"unreliable\"." +
  LANGUAGE_RULE + "\n\nReturn structured output only."

const VERIFY_PROMPT = (claim, v) =>
  "## Adversarial claim verification (vote " + (v + 1) + " of " + VOTES_PER_CLAIM + ")\n\n" +
  "Stay skeptical. Try hard to **refute** this claim. " + REFUTATIONS_REQUIRED + "/" + VOTES_PER_CLAIM + " refutations eliminate it.\n\n" +
  "## Research question\n" + QUESTION + "\n\n" +
  "## Claim under review\n" + claim.claim + "\n\n" +
  "**Source:** " + claim.sourceUrl + " (" + claim.sourceQuality + ")\n" +
  "**Supporting quote:** " + claim.quote + "\n\n" +
  "## Checklist\n" +
  "1. Does the quote actually support the claim, or is it an overreading or a misreading?\n" +
  "2. Use WebSearch to look for counter-evidence — does a credible source contradict or heavily qualify it?\n" +
  "3. Is the source quality good enough for the strength of the claim? (Stronger claims need primary sources.)\n" +
  "4. Is the claim out of date? (Check dates; old conclusions in fast-moving fields are especially suspect.)\n" +
  "5. Is this marketing copy, a press release, cherry-picked benchmarks, or forum speculation?\n\n" +
  "**refuted=true** when: the quote does not support it / it is contradicted / the source cannot carry a claim that strong / it is out of date / it is marketing.\n" +
  "**refuted=false** only when the claim is well supported, still current, and the source quality matches the strength of the assertion.\n" +
  "When in doubt, mark refuted=true." +
  LANGUAGE_RULE + "\n\nReturn structured output only. Evidence must be specific."

// --- Pipeline: Search -> dedupe -> Fetch+extract (two stages, no barrier) ---
// Both stages announce themselves. Declaring "Search" and "Fetch" in
// meta.phases and then only tagging individual agents with them left the run's
// reported phase sitting on "Scope" from the first search until verification
// began — the whole expensive middle of the run, and exactly the window a
// wall-clock handoff or a watchdog kill lands in, named the wrong stage.
// The two stages genuinely interleave (pipeline has no barrier), so "Fetch" is
// announced once, when the first source is actually dispatched: by then
// searching is effectively done and fetching is what the run is spending its
// time on.
phase("Search")
let announcedFetchPhase = false
const searchResults = await pipeline(
  scope.angles,

  angle => ask(SEARCH_PROMPT(angle), {
    label: "search:" + angle.label, phase: "Search", schema: SEARCH_SCHEMA
  }).then(r => {
    if (!r) return null
    log(angle.label + ": " + r.results.length + " results")
    return { angle: angle.label, results: r.results }
  }),

  searchResult => {
    // Stage 1 hands its return value straight through, so a skipped or failed
    // search agent arrives here as null.
    if (!searchResult) return []
    const sorted = [...searchResult.results].sort((a, b) => relRank[a.relevance] - relRank[b.relevance])
    const novel = sorted.filter(r => {
      const key = normURL(r.url)
      if (seen.has(key)) {
        dupes.push({ ...r, angle: searchResult.angle, dupOf: seen.get(key) })
        return false
      }
      if (fetched >= MAX_FETCH_HARD) {
        budgetDropped.push({ ...r, angle: searchResult.angle })
        return false
      }
      if (fetchSlots <= 0 && relRank[r.relevance] >= 1) {
        budgetDropped.push({ ...r, angle: searchResult.angle })
        return false
      }
      seen.set(key, { angle: searchResult.angle, title: r.title })
      fetchSlots--
      fetched++
      return true
    })
    if (novel.length < searchResult.results.length) {
      log(searchResult.angle + ": " + novel.length + " new sources (" + (searchResult.results.length - novel.length) + " filtered out)")
    }
    if (novel.length > 0 && !announcedFetchPhase) {
      announcedFetchPhase = true
      phase("Fetch")
    }
    return parallel(
      novel.map(source => () => {
        const host = urlHost(source.url) || "unknown"
        return ask(FETCH_PROMPT(source, searchResult.angle), {
          label: "fetch:" + host,
          phase: "Fetch",
          schema: EXTRACT_SCHEMA,
        }).then(ext => {
          // A skip and a failure both arrive as null; drop them (filter(Boolean)
          // catches it) rather than faking an "unreliable" source that would
          // pollute the statistics.
          if (!ext) return null
          return {
            url: source.url, title: source.title, angle: searchResult.angle,
            sourceQuality: ext.sourceQuality, publishDate: ext.publishDate,
            claims: ext.claims.map(c => ({ ...c, sourceUrl: source.url, sourceQuality: ext.sourceQuality })),
          }
        })
      })
    )
  }
)

const allSources = searchResults.flat().filter(Boolean)
const allClaims = allSources.flatMap(s => s.claims)
const impRank = { central: 0, supporting: 1, tangential: 2 }
const qualRank = { primary: 0, secondary: 1, blog: 2, forum: 3, unreliable: 4 }

const rankedClaims = [...allClaims]
  .sort((a, b) => (impRank[a.importance] - impRank[b.importance]) || (qualRank[a.sourceQuality] - qualRank[b.sourceQuality]))
  .slice(0, MAX_VERIFY_CLAIMS)

log("Fetched " + allSources.length + " sources -> " + allClaims.length + " claims -> " + rankedClaims.length + " going into verification")
if (allClaims.length > rankedClaims.length) {
  log("After ranking by importance and source quality, " + (allClaims.length - rankedClaims.length) + " claims did not enter verification (budget cap " + MAX_VERIFY_CLAIMS + ")")
}

if (rankedClaims.length === 0) {
  return {
    question: QUESTION,
    summary: "No claims were extracted. " + allSources.length + " sources were fetched and all of them were empty or failed." +
      (allSources.length === 0
        ? " The search stage returned no sources at all — if this machine has no working web search (no search provider configured, and the current model cannot search the web), this workflow cannot run."
        : " The sources were most likely paywalled, empty, or irrelevant.") +
      " " + dupes.length + " duplicate URLs, " + budgetDropped.length + " dropped for budget.",
    findings: [], refuted: [], sources: allSources.map(s => ({ url: s.url, quality: s.sourceQuality })),
    stats: { angles: scope.angles.length, sources: allSources.length, claims: 0, dupes: dupes.length },
  }
}

// --- Verify: 3 adversarial votes ---
// Verification is the most expensive stage (VOTES_PER_CLAIM agents per claim
// against a 3-wide scheduler), so it is the one place where entering with no
// budget left guarantees a wasted, truncated run. Report what the fetch stage
// already established instead.
if (deadline.exceeded()) {
  return {
    question: QUESTION,
    status: "incomplete",
    summary: "The run reached its time budget after fetching sources but before adversarial verification, so these claims are unverified. " +
      allSources.length + " sources produced " + allClaims.length + " claims.",
    findings: [],
    unverifiedClaims: rankedClaims.map(c => ({ claim: c.claim, source: c.sourceUrl, quote: c.quote, quality: c.sourceQuality })),
    sources: allSources.map(s => ({ url: s.url, quality: s.sourceQuality, claimCount: s.claims.length })),
    stats: { angles: scope.angles.length, sources: allSources.length, claims: allClaims.length, verified: 0, urlDupes: dupes.length, budgetDropped: budgetDropped.length },
  }
}

// The barrier here is deliberate: the full claim pool has to be collected
// before it can be ranked and the best claims selected for verification.
phase("Verify")
const voted = (await parallel(
  rankedClaims.map(claim => () =>
    parallel(
      Array.from({ length: VOTES_PER_CLAIM }, (_, v) => () =>
        ask(VERIFY_PROMPT(claim, v), {
          label: "v" + v + ":" + claim.claim.slice(0, 40),
          phase: "Verify",
          schema: VERDICT_SCHEMA,
        })
      )
    ).then(verdicts => {
      // An individual vote can be null (user skip or agent failure) — treat it
      // as an abstention.
      const valid = verdicts.filter(Boolean)
      const refuted = valid.filter(v => v.refuted).length
      // Survival requires having actually been adjudicated: a quorum of valid
      // votes AND fewer refutations than the threshold. Too many abstentions =
      // unverified, and must never reach the report (otherwise an all-abstain
      // claim survives on refuted === 0).
      const abstained = VOTES_PER_CLAIM - valid.length
      const survives = valid.length >= REFUTATIONS_REQUIRED && refuted < REFUTATIONS_REQUIRED
      log("\"" + claim.claim.slice(0, 50) + "…\": " + (valid.length - refuted) + "-" + refuted + (abstained > 0 ? " (" + abstained + " abstained)" : "") + " " + (survives ? "✓" : "✗"))
      return { ...claim, verdicts: valid, refutedVotes: refuted, survives }
    })
  )
)).filter(Boolean)

// Adjudication actually lost, not merely "some votes were null": each claim's
// missing votes summed. Reported alongside the verdicts so a reader can tell a
// 3-0 from a 2-0-with-one-vote-missing.
const abstainedVotes = voted.reduce((sum, c) => sum + (VOTES_PER_CLAIM - c.verdicts.length), 0)
const confirmed = voted.filter(c => c.survives)
const killed = voted.filter(c => !c.survives)
log("Verification complete: " + voted.length + " claims -> " + confirmed.length + " survived, " + killed.length + " eliminated")

if (confirmed.length === 0) {
  return {
    question: QUESTION,
    summary: "All " + voted.length + " claims were eliminated by adversarial verification. No research conclusion stands — the sources were probably low quality, or the claims were overstated.",
    findings: [],
    refuted: killed.map(c => ({ claim: c.claim, vote: (c.verdicts.length - c.refutedVotes) + "-" + c.refutedVotes, source: c.sourceUrl })),
    sources: allSources.map(s => ({ url: s.url, quality: s.sourceQuality, claimCount: s.claims.length })),
    stats: { angles: scope.angles.length, sources: allSources.length, claims: allClaims.length, verified: voted.length, confirmed: 0, killed: killed.length, degradedAgentCalls: degradedTotal(), abstainedVotes },
  }
}

// --- Synthesize ---
// Synthesis is a single agent, so it is worth attempting even on a thin
// budget; only skip it once the budget is actually gone, and hand back the
// verified claims unmerged rather than losing them.
if (deadline.exceeded()) {
  return {
    question: QUESTION,
    status: "incomplete",
    summary: "The run reached its time budget after verification but before synthesis — returning the " + confirmed.length + " verified claims directly, unmerged.",
    findings: [],
    confirmed: confirmed.map(c => ({ claim: c.claim, source: c.sourceUrl, quote: c.quote, vote: (c.verdicts.length - c.refutedVotes) + "-" + c.refutedVotes })),
    refuted: killed.map(c => ({ claim: c.claim, vote: (c.verdicts.length - c.refutedVotes) + "-" + c.refutedVotes, source: c.sourceUrl })),
    sources: allSources.map(s => ({ url: s.url, quality: s.sourceQuality, claimCount: s.claims.length })),
    stats: { angles: scope.angles.length, sources: allSources.length, claims: allClaims.length, verified: voted.length, confirmed: confirmed.length, killed: killed.length, afterSynthesis: 0, degradedAgentCalls: degradedTotal(), abstainedVotes },
  }
}

phase("Synthesize")
const confRank = { high: 0, medium: 1, low: 2 }
const block = confirmed.map((c, i) => {
  const best = c.verdicts.filter(v => !v.refuted).sort((a, b) => confRank[a.confidence] - confRank[b.confidence])[0]
  return "### [" + i + "] " + c.claim + "\n" +
    "Vote: " + (c.verdicts.length - c.refutedVotes) + "-" + c.refutedVotes + " · Source: " + c.sourceUrl + " (" + c.sourceQuality + ")\n" +
    "Quote: " + c.quote + "\nVerifier evidence (" + best.confidence + "): " + best.evidence + "\n"
}).join("\n")

const killedBlock = killed.length > 0
  ? "\n## Eliminated claims (for transparency)\n" +
    killed.map(c => "- " + c.claim + " (" + c.sourceUrl + ", vote " + (c.verdicts.length - c.refutedVotes) + "-" + c.refutedVotes + ")").join("\n")
  : ""

const report = await ask(
  "## Synthesis: write the research report\n\n" +
  "**Research question:** " + QUESTION + "\n\n" +
  confirmed.length + " claims survived " + VOTES_PER_CLAIM + "-vote adversarial verification. Merge semantic duplicates and synthesise them into a report.\n\n" +
  "## Confirmed claims\n" + block + "\n" + killedBlock + "\n\n" +
  "## Requirements\n" +
  "1. Find claims that say the same thing — merge them, and merge their sources.\n" +
  "2. Group related claims into clear conclusions, each of which answers the research question directly.\n" +
  "3. Mark a confidence per finding: high (multiple primary sources, unanimous vote), medium (secondary sources or a split vote), low (single source or blog-grade quality).\n" +
  "4. Write a 3-5 sentence executive summary that answers the research question directly.\n" +
  "5. State the limitations: what remains uncertain, which sources are weak, which conclusions are time-sensitive.\n" +
  "6. List 2-4 questions that surfaced but remain unanswered." +
  LANGUAGE_RULE + "\n\nReturn structured output only.",
  { label: "synthesize", schema: REPORT_SCHEMA }
)

if (!report) {
  // The synthesis step was skipped or failed — hand back the verified claims
  // rather than losing the entire run with them.
  return {
    question: QUESTION,
    summary: "The synthesis step was skipped or failed — returning the " + confirmed.length + " verified claims directly, unmerged.",
    findings: [],
    confirmed: confirmed.map(c => ({ claim: c.claim, source: c.sourceUrl, quote: c.quote, vote: (c.verdicts.length - c.refutedVotes) + "-" + c.refutedVotes })),
    refuted: killed.map(c => ({ claim: c.claim, vote: (c.verdicts.length - c.refutedVotes) + "-" + c.refutedVotes, source: c.sourceUrl })),
    sources: allSources.map(s => ({ url: s.url, quality: s.sourceQuality, claimCount: s.claims.length })),
    stats: { angles: scope.angles.length, sources: allSources.length, claims: allClaims.length, verified: voted.length, confirmed: confirmed.length, killed: killed.length, afterSynthesis: 0, degradedAgentCalls: degradedTotal(), abstainedVotes },
  }
}

return {
  question: QUESTION,
  ...report,
  refuted: killed.map(c => ({ claim: c.claim, vote: (c.verdicts.length - c.refutedVotes) + "-" + c.refutedVotes, source: c.sourceUrl })),
  sources: allSources.map(s => ({ url: s.url, quality: s.sourceQuality, angle: s.angle, claimCount: s.claims.length })),
  stats: {
    angles: scope.angles.length,
    sourcesFetched: allSources.length,
    claimsExtracted: allClaims.length,
    claimsVerified: voted.length,
    confirmed: confirmed.length,
    killed: killed.length,
    afterSynthesis: report.findings.length,
    urlDupes: dupes.length,
    budgetDropped: budgetDropped.length,
    agentCalls: 1 + scope.angles.length + allSources.length + (voted.length * VOTES_PER_CLAIM) + 1,
    // Coverage actually lost. agentCalls above counts what was *planned*;
    // these two say how much of it produced nothing, so the reader can judge
    // the report evidence base instead of assuming the plan was executed.
    degradedAgentCalls: degradedTotal(),
    abstainedVotes,
  },
}
`
