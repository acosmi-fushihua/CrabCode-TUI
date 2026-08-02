import { readdir } from 'fs/promises'
import { basename, extname } from 'path'
import { logForDebugging } from '../../utils/debug.js'
import { loadAllPluginsCacheOnly } from '../../utils/plugins/pluginLoader.js'
import {
  PluginPathSecurityError,
  readCanonicalPluginTextFile,
  resolvePluginComponentPath,
} from '../../utils/plugins/pluginPathSecurity.js'
import {
  parseWorkflowModule,
  type WorkflowMeta,
} from './parser.js'
import { WORKFLOW_MAX_SOURCE_BYTES } from './constants.js'

export {
  parseWorkflowModule,
  type ParsedWorkflowModule,
  type WorkflowMeta,
  type WorkflowPhaseMeta,
} from './parser.js'

/**
 * Where an executable workflow came from.
 *
 * `plugin` — discovered on disk under an enabled plugin's `workflows/` dir.
 * `bundled` — shipped inside the CrabCode executable and registered at
 * startup by `initBundledWorkflows()`. Bundled entries carry no real
 * filesystem path, so anything that resolves paths must branch on this
 * instead of assuming `pluginPath` is a directory.
 */
export type WorkflowOrigin = 'plugin' | 'bundled'

export type DiscoveredWorkflow = {
  name: string
  localName: string
  origin: WorkflowOrigin
  pluginName: string
  pluginSource: string
  pluginPath: string
  filePath: string
  source: string
  executableSource: string
  meta: WorkflowMeta
}

export type WorkflowDiscoveryResult = {
  workflows: DiscoveredWorkflow[]
  errors: string[]
}

export type WorkflowPluginSource = {
  name: string
  source: string
  path: string
}

export async function discoverPluginWorkflows(
  targetCwd?: string,
): Promise<WorkflowDiscoveryResult> {
  const { enabled, errors: pluginErrors } =
    await loadAllPluginsCacheOnly(targetCwd)
  return discoverPluginWorkflowsFromPlugins(
    enabled,
    pluginErrors.map(error => JSON.stringify(error)),
  )
}

/**
 * Filesystem discovery core, split from installed-plugin lookup so the
 * namespace/path contract can be tested without mutating global plugin
 * settings or replacing live ESM modules.
 */
export async function discoverPluginWorkflowsFromPlugins(
  plugins: readonly WorkflowPluginSource[],
  initialErrors: readonly string[] = [],
): Promise<WorkflowDiscoveryResult> {
  const workflows: DiscoveredWorkflow[] = []
  const errors = [...initialErrors]
  const names = new Set<string>()

  for (const plugin of plugins) {
    let workflowsDir: string
    try {
      workflowsDir = await resolvePluginComponentPath(
        plugin.path,
        'workflows',
        { component: 'workflows' },
      )
    } catch (error) {
      if (
        error instanceof PluginPathSecurityError &&
        error.reason === 'path-missing'
      ) {
        continue
      }
      const message = `${plugin.name}: cannot load workflows directory: ${
        error instanceof Error ? error.message : String(error)
      }`
      errors.push(message)
      logForDebugging(message, { level: 'warn' })
      continue
    }

    let entries
    try {
      entries = await readdir(workflowsDir, { withFileTypes: true })
    } catch (error) {
      const message = `${plugin.name}: cannot list workflows: ${
        error instanceof Error ? error.message : String(error)
      }`
      errors.push(message)
      logForDebugging(message, { level: 'warn' })
      continue
    }

    for (const entry of entries.sort((a, b) =>
      a.name.localeCompare(b.name),
    )) {
      if (!entry.isFile() || extname(entry.name) !== '.js') continue
      try {
        const filePath = await resolvePluginComponentPath(
          plugin.path,
          `workflows/${entry.name}`,
          { component: 'workflow script' },
        )
        const source = await readCanonicalPluginTextFile(
          plugin.path,
          filePath,
          'workflow script',
          { maxBytes: WORKFLOW_MAX_SOURCE_BYTES },
        )
        const parsed = parseWorkflowModule(source, filePath)
        const fileName = basename(entry.name, '.js')
        if (parsed.meta.name !== fileName) {
          throw new Error(
            `${filePath}: meta.name "${parsed.meta.name}" must match filename "${fileName}"`,
          )
        }
        const name = `${plugin.name}:${parsed.meta.name}`
        if (names.has(name)) {
          throw new Error(`duplicate workflow name: ${name}`)
        }
        names.add(name)
        workflows.push({
          name,
          localName: parsed.meta.name,
          origin: 'plugin',
          pluginName: plugin.name,
          pluginSource: plugin.source,
          pluginPath: plugin.path,
          filePath,
          source,
          executableSource: parsed.executableSource,
          meta: parsed.meta,
        })
      } catch (error) {
        const message = `${plugin.name}/${entry.name}: ${
          error instanceof Error ? error.message : String(error)
        }`
        errors.push(message)
        logForDebugging(message, { level: 'warn' })
      }
    }
  }

  workflows.sort((a, b) => a.name.localeCompare(b.name))
  return { workflows, errors }
}

/**
 * Every executable workflow visible to this process: the ones bundled into the
 * binary plus the ones discovered under enabled plugins.
 *
 * Bundled entries come first and win on name collision. A plugin cannot
 * shadow a bundled workflow by declaring the same bare name, because bundled
 * names are un-namespaced while plugin names are always `<plugin>:<name>` —
 * the collision check below is therefore a defensive invariant, not an
 * expected path.
 */
export async function discoverWorkflows(
  targetCwd?: string,
): Promise<WorkflowDiscoveryResult> {
  const { getBundledWorkflows, getBundledWorkflowErrors, initBundledWorkflows } =
    await import('./bundled/index.js')
  // Registration must not depend on module evaluation order. `tools.ts` also
  // calls this while building the tool list, but discovery can be reached
  // first through the command factory, and an unregistered catalog would make
  // the bundled workflow silently vanish rather than fail loudly. The call is
  // idempotent.
  initBundledWorkflows()
  const bundled = getBundledWorkflows()
  const plugin = await discoverPluginWorkflows(targetCwd)
  const names = new Set(bundled.map(workflow => workflow.name))
  const errors = [...getBundledWorkflowErrors(), ...plugin.errors]
  const workflows = [...bundled]
  for (const workflow of plugin.workflows) {
    if (names.has(workflow.name)) {
      errors.push(
        `${workflow.filePath}: workflow name "${workflow.name}" is reserved by a bundled workflow`,
      )
      continue
    }
    names.add(workflow.name)
    workflows.push(workflow)
  }
  return { workflows, errors }
}

export async function findWorkflow(
  name: string,
  targetCwd?: string,
): Promise<{
  workflow: DiscoveredWorkflow | null
  available: string[]
  errors: string[]
}> {
  const result = await discoverWorkflows(targetCwd)
  return {
    workflow: result.workflows.find(workflow => workflow.name === name) ?? null,
    available: result.workflows.map(workflow => workflow.name),
    errors: result.errors,
  }
}
