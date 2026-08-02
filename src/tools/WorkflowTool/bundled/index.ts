/**
 * Bundled workflow registry.
 *
 * Bundled workflows ship *inside* the CrabCode executable rather than being
 * discovered on disk under an enabled plugin. They exist because a workflow
 * can be a first-party product feature (`deep-research`) rather than
 * third-party installed code.
 *
 * Why they are not modelled as built-in plugins: `getBuiltinPlugins()` hands
 * back `path: 'builtin'` — an explicit sentinel, not a directory — while
 * workflow discovery is filesystem-based (`resolvePluginComponentPath`). A
 * built-in plugin therefore resolves to `path-missing` and is skipped. The
 * only alternative would be materialising scripts onto disk at startup, which
 * buys a write path, a packaging-integrity question and a tamper surface for
 * nothing.
 *
 * Registration parses eagerly so that a malformed bundled script is caught by
 * the test suite rather than at a user's first invocation. A parse failure at
 * runtime is recorded and surfaced through discovery's `errors[]` (the same
 * channel plugin load failures use) instead of throwing, so one bad bundled
 * script cannot take CLI startup down with it.
 */

import { logForDebugging } from '../../../utils/debug.js'
import { parseWorkflowModule } from '../parser.js'
import type { DiscoveredWorkflow } from '../registry.js'
import {
  DEEP_RESEARCH_WORKFLOW_NAME,
  DEEP_RESEARCH_WORKFLOW_SOURCE,
} from './deepResearch.js'

/**
 * Display namespace for bundled workflows. Deliberately not a plugin name —
 * nothing resolves it against the plugin registry.
 */
export const BUNDLED_WORKFLOW_PLUGIN_NAME = 'crabcode'

/**
 * Ownership identity for bundled workflows, mirroring `pluginSource` for
 * plugin-owned ones. `runtime.ts::isAgentOwnedByWorkflow` matches on
 * `origin` rather than on this string, so it is an identity label and not a
 * security boundary.
 */
export const BUNDLED_WORKFLOW_SOURCE = 'bundled'

const BUNDLED_WORKFLOWS = new Map<string, DiscoveredWorkflow>()
const BUNDLED_WORKFLOW_ERRORS: string[] = []

/**
 * The bundled catalog. Adding an entry here is the whole registration step —
 * discovery, the tool catalog, lookup and the slash command all follow.
 */
const BUNDLED_WORKFLOW_SOURCES: ReadonlyArray<readonly [string, string]> = [
  [DEEP_RESEARCH_WORKFLOW_NAME, DEEP_RESEARCH_WORKFLOW_SOURCE],
]

/**
 * Register one bundled workflow from its module source text.
 *
 * Throws on a malformed script: `initBundledWorkflows` converts that into a
 * recorded error, but tests calling this directly should see the failure.
 */
export function registerBundledWorkflow(name: string, source: string): void {
  const filePath = `<bundled>/${name}.js`
  const parsed = parseWorkflowModule(source, filePath)
  if (parsed.meta.name !== name) {
    throw new Error(
      `${filePath}: workflow meta.name "${parsed.meta.name}" must match registered name "${name}"`,
    )
  }
  if (BUNDLED_WORKFLOWS.has(name)) {
    throw new Error(`duplicate bundled workflow: ${name}`)
  }
  BUNDLED_WORKFLOWS.set(name, {
    // Bundled workflows are un-namespaced: `deep-research`, not
    // `crabcode:deep-research`. Plugin workflows are always `<plugin>:<name>`,
    // so the two namespaces cannot collide.
    name,
    localName: name,
    origin: 'bundled',
    pluginName: BUNDLED_WORKFLOW_PLUGIN_NAME,
    pluginSource: BUNDLED_WORKFLOW_SOURCE,
    pluginPath: BUNDLED_WORKFLOW_SOURCE,
    filePath,
    source,
    executableSource: parsed.executableSource,
    meta: parsed.meta,
  })
}

export function getBundledWorkflows(): DiscoveredWorkflow[] {
  return [...BUNDLED_WORKFLOWS.values()].sort((a, b) =>
    a.name.localeCompare(b.name),
  )
}

export function getBundledWorkflowErrors(): string[] {
  return [...BUNDLED_WORKFLOW_ERRORS]
}

/** Test seam — production code never needs to unregister. */
export function clearBundledWorkflows(): void {
  BUNDLED_WORKFLOWS.clear()
  BUNDLED_WORKFLOW_ERRORS.length = 0
}

/**
 * Initialize bundled workflows. Called once during tool registration.
 * Idempotent: repeated calls (tests, re-entrant tool setup) are no-ops.
 */
export function initBundledWorkflows(): void {
  if (BUNDLED_WORKFLOWS.size > 0 || BUNDLED_WORKFLOW_ERRORS.length > 0) {
    return
  }
  for (const [name, source] of BUNDLED_WORKFLOW_SOURCES) {
    try {
      registerBundledWorkflow(name, source)
    } catch (error) {
      const message = `bundled workflow ${name}: ${
        error instanceof Error ? error.message : String(error)
      }`
      BUNDLED_WORKFLOW_ERRORS.push(message)
      logForDebugging(message, { level: 'warn' })
    }
  }
}
