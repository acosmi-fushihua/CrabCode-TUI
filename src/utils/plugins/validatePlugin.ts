import type { Dirent, Stats } from 'fs'
import { readdir, readFile, stat } from 'fs/promises'
import * as path from 'path'
import { z } from 'zod/v4'
import { errorMessage, getErrnoCode, isENOENT } from '../errors.js'
import { FRONTMATTER_REGEX } from '../frontmatterParser.js'
import { jsonParse } from '../slowOperations.js'
import { parseYaml } from '../yaml.js'
import {
  PluginPathSecurityError,
  readCanonicalPluginTextFile,
  revalidatePluginPath,
  resolveInternalPluginPath,
  resolvePluginComponentPath,
} from './pluginPathSecurity.js'
import {
  PluginHooksSchema,
  PluginManifestSchema,
  PluginMarketplaceEntrySchema,
  PluginMarketplaceSchema,
} from './schemas.js'

/**
 * Fields that belong in marketplace.json entries (PluginMarketplaceEntrySchema)
 * but not plugin.json (PluginManifestSchema). Plugin authors reasonably copy
 * one into the other. Surfaced as warnings by `crabcode plugin validate` since
 * they're a known confusion point — the load path silently strips all unknown
 * keys via zod's default behavior, so they're harmless at runtime but worth
 * flagging to authors.
 */
const MARKETPLACE_ONLY_MANIFEST_FIELDS = new Set([
  'category',
  'source',
  'tags',
  'strict',
  'id',
])

export type ValidationResult = {
  success: boolean
  errors: ValidationError[]
  warnings: ValidationWarning[]
  filePath: string
  fileType: 'plugin' | 'marketplace' | 'skill' | 'agent' | 'command' | 'hooks'
}

export type ValidationError = {
  path: string
  message: string
  code?: string
}

export type ValidationWarning = {
  path: string
  message: string
}

/**
 * Detect whether a file is a plugin manifest or marketplace manifest
 */
function detectManifestType(
  filePath: string,
): 'plugin' | 'marketplace' | 'unknown' {
  const fileName = path.basename(filePath)
  const dirName = path.basename(path.dirname(filePath))

  // Check filename patterns
  if (fileName === 'plugin.json') return 'plugin'
  if (fileName === 'marketplace.json') return 'marketplace'

  // Check if it's in .crabcode-plugin directory
  if (dirName === '.crabcode-plugin') {
    return 'plugin' // Most likely plugin.json
  }

  return 'unknown'
}

/**
 * Format Zod validation errors into a readable format
 */
function formatZodErrors(zodError: z.ZodError): ValidationError[] {
  return zodError.issues.map(error => ({
    path: error.path.join('.') || 'root',
    message: error.message,
    code: error.code,
  }))
}

/**
 * Check for parent-directory segments ('..') in a path string.
 *
 * For plugin.json component paths this is a security concern (escaping the plugin dir).
 * For marketplace.json source paths it's almost always a resolution-base misunderstanding:
 * paths resolve from the marketplace repo root, not from marketplace.json itself, so the
 * '..' a user added to "climb out of .crabcode-plugin/" is unnecessary. Callers pass `hint`
 * to attach the right explanation.
 */
function checkPathTraversal(
  p: string,
  field: string,
  errors: ValidationError[],
  hint?: string,
): void {
  if (p.includes('..')) {
    errors.push({
      path: field,
      message: hint
        ? `Path contains "..": ${p}. ${hint}`
        : `Path contains ".." which could be a path traversal attempt: ${p}`,
    })
  }
}

function collectManifestComponentPaths(
  manifest: Record<string, unknown>,
): Array<{ field: string; value: string }> {
  const paths: Array<{ field: string; value: string }> = []
  const addStrings = (field: string, value: unknown): void => {
    const values = Array.isArray(value) ? value : [value]
    values.forEach((item, index) => {
      if (typeof item === 'string') {
        if (!/^https?:\/\//i.test(item)) {
          paths.push({ field: `${field}[${index}]`, value: item })
        }
      }
    })
  }

  const commands = manifest.commands
  if (
    commands &&
    typeof commands === 'object' &&
    !Array.isArray(commands)
  ) {
    for (const [name, metadata] of Object.entries(commands)) {
      if (
        metadata &&
        typeof metadata === 'object' &&
        typeof (metadata as { source?: unknown }).source === 'string'
      ) {
        paths.push({
          field: `commands.${name}.source`,
          value: (metadata as { source: string }).source,
        })
      }
    }
  } else if (commands) {
    addStrings('commands', commands)
  }

  for (const field of [
    'agents',
    'skills',
    'outputStyles',
    'hooks',
    'mcpServers',
    'lspServers',
  ] as const) {
    const value = manifest[field]
    if (value) addStrings(field, value)
  }
  return paths
}

function getManifestRoot(filePath: string): string {
  const parent = path.dirname(filePath)
  return path.basename(parent) === '.crabcode-plugin'
    ? path.dirname(parent)
    : parent
}

async function resolveManifestFileForValidation(
  filePath: string,
  component: string,
): Promise<{ root: string; filePath: string }> {
  const root = getManifestRoot(filePath)
  const requestedPath = path.relative(root, filePath)
  return {
    root,
    filePath: await resolvePluginComponentPath(root, requestedPath, {
      component,
    }),
  }
}

async function validateManifestComponentPaths(
  pluginRoot: string,
  manifest: Record<string, unknown>,
  errors: ValidationError[],
  prefix = '',
): Promise<void> {
  for (const { field, value } of collectManifestComponentPaths(manifest)) {
    try {
      await resolvePluginComponentPath(pluginRoot, value, {
        component: `plugin manifest ${field}`,
      })
    } catch (error) {
      errors.push({
        path: prefix ? `${prefix}.${field}` : field,
        message:
          error instanceof PluginPathSecurityError
            ? error.message
            : `Component path cannot be validated: ${errorMessage(error)}`,
        code:
          error instanceof PluginPathSecurityError ? error.reason : undefined,
      })
    }
  }
}

async function validateDefaultPluginPaths(
  pluginRoot: string,
  errors: ValidationError[],
): Promise<void> {
  for (const requestedPath of [
    'commands',
    'agents',
    'skills',
    'output-styles',
    'hooks/hooks.json',
    'settings.json',
    '.mcp.json',
    '.lsp.json',
  ]) {
    try {
      await resolvePluginComponentPath(pluginRoot, requestedPath, {
        component: `default plugin path ${requestedPath}`,
      })
    } catch (error) {
      if (
        error instanceof PluginPathSecurityError &&
        error.reason === 'path-missing'
      ) {
        continue
      }
      errors.push({
        path: requestedPath,
        message:
          error instanceof PluginPathSecurityError
            ? error.message
            : `Default plugin path cannot be validated: ${errorMessage(error)}`,
        code:
          error instanceof PluginPathSecurityError ? error.reason : undefined,
      })
    }
  }
}

// Shown when a marketplace plugin source contains '..'. Most users hit this because
// they expect paths to resolve relative to marketplace.json (inside .crabcode-plugin/),
// but resolution actually starts at the marketplace repo root — see gh-29485.
// Computes a tailored "use X instead of Y" suggestion from the user's actual path
// rather than a hardcoded example (review feedback on #20895).
function marketplaceSourceHint(p: string): string {
  // Strip leading ../ segments: the '..' a user added to "climb out of
  // .crabcode-plugin/" is unnecessary since paths already start at the repo root.
  // If '..' appears mid-path (rare), fall back to a generic example.
  const stripped = p.replace(/^(\.\.\/)+/, '')
  const corrected = stripped !== p ? `./${stripped}` : './plugins/my-plugin'
  return (
    'Plugin source paths are resolved relative to the marketplace root (the directory ' +
    'containing .crabcode-plugin/), not relative to marketplace.json. ' +
    `Use "${corrected}" instead of "${p}".`
  )
}

/**
 * Validate a plugin manifest file (plugin.json)
 */
export async function validatePluginManifest(
  filePath: string,
): Promise<ValidationResult> {
  const errors: ValidationError[] = []
  const warnings: ValidationWarning[] = []
  const absolutePath = path.resolve(filePath)

  // Read file content — handle ENOENT / EISDIR / permission errors directly
  let content: string
  let pluginRoot = getManifestRoot(absolutePath)
  try {
    const resolved = await resolveManifestFileForValidation(
      absolutePath,
      'plugin manifest',
    )
    pluginRoot = resolved.root
    content = await readCanonicalPluginTextFile(
      pluginRoot,
      resolved.filePath,
      'plugin manifest',
    )
  } catch (error: unknown) {
    const code =
      error instanceof PluginPathSecurityError && error.reason === 'path-missing'
        ? 'ENOENT'
        : getErrnoCode(error)
    let message: string
    if (code === 'ENOENT') {
      message = `File not found: ${absolutePath}`
    } else if (code === 'EISDIR') {
      message = `Path is not a file: ${absolutePath}`
    } else {
      message = `Failed to read file: ${errorMessage(error)}`
    }
    return {
      success: false,
      errors: [{ path: 'file', message, code }],
      warnings: [],
      filePath: absolutePath,
      fileType: 'plugin',
    }
  }

  let parsed: unknown
  try {
    parsed = jsonParse(content)
  } catch (error) {
    return {
      success: false,
      errors: [
        {
          path: 'json',
          message: `Invalid JSON syntax: ${errorMessage(error)}`,
        },
      ],
      warnings: [],
      filePath: absolutePath,
      fileType: 'plugin',
    }
  }

  // Check for path traversal in the parsed JSON before schema validation
  // This ensures we catch security issues even if schema validation fails
  if (parsed && typeof parsed === 'object') {
    const obj = parsed as Record<string, unknown>

    // Check commands
    if (obj.commands) {
      const commands = Array.isArray(obj.commands)
        ? obj.commands
        : [obj.commands]
      commands.forEach((cmd, i) => {
        if (typeof cmd === 'string') {
          checkPathTraversal(cmd, `commands[${i}]`, errors)
        }
      })
    }

    // Check agents
    if (obj.agents) {
      const agents = Array.isArray(obj.agents) ? obj.agents : [obj.agents]
      agents.forEach((agent, i) => {
        if (typeof agent === 'string') {
          checkPathTraversal(agent, `agents[${i}]`, errors)
        }
      })
    }

    // Check skills
    if (obj.skills) {
      const skills = Array.isArray(obj.skills) ? obj.skills : [obj.skills]
      skills.forEach((skill, i) => {
        if (typeof skill === 'string') {
          checkPathTraversal(skill, `skills[${i}]`, errors)
        }
      })
    }

    await validateManifestComponentPaths(pluginRoot, obj, errors)
    await validateDefaultPluginPaths(pluginRoot, errors)
  }

  // Surface marketplace-only fields as a warning BEFORE validation flags
  // them. `crabcode plugin validate` is a developer tool — authors running it
  // want to know these fields don't belong here. But it's a warning, not an
  // error: the plugin loads fine at runtime (the base schema strips unknown
  // keys). We strip them here so the .strict() call below doesn't double-
  // report them as unrecognized-key errors on top of the targeted warnings.
  let toValidate = parsed
  if (typeof parsed === 'object' && parsed !== null) {
    const obj = parsed as Record<string, unknown>
    const strayKeys = Object.keys(obj).filter(k =>
      MARKETPLACE_ONLY_MANIFEST_FIELDS.has(k),
    )
    if (strayKeys.length > 0) {
      const stripped = { ...obj }
      for (const key of strayKeys) {
        delete stripped[key]
        warnings.push({
          path: key,
          message:
            `Field '${key}' belongs in the marketplace entry (marketplace.json), ` +
            `not plugin.json. It's harmless here but unused — CrabCode ` +
            `ignores it at load time.`,
        })
      }
      toValidate = stripped
    }
  }

  // Validate against schema (post-strip, so marketplace fields don't fail it).
  // We call .strict() locally here even though the base schema is lenient —
  // the runtime load path silently strips unknown keys for resilience, but
  // this is a developer tool and authors running it want typo feedback.
  const result = PluginManifestSchema().strict().safeParse(toValidate)

  if (!result.success) {
    errors.push(...formatZodErrors(result.error))
  }

  // Check for common issues and add warnings
  if (result.success) {
    const manifest = result.data

    // Warn if name isn't strict kebab-case. CC's schema only rejects spaces,
    // but the Acosmi marketplace sync rejects non-kebab names. Surfacing
    // this here lets authors catch it in CI before the sync fails on them.
    if (!/^[a-z0-9]+(-[a-z0-9]+)*$/.test(manifest.name)) {
      warnings.push({
        path: 'name',
        message:
          `Plugin name "${manifest.name}" is not kebab-case. CrabCode accepts ` +
          `it, but the Acosmi marketplace sync requires kebab-case ` +
          `(lowercase letters, digits, and hyphens only, e.g., "my-plugin").`,
      })
    }

    // Warn if no version specified
    if (!manifest.version) {
      warnings.push({
        path: 'version',
        message:
          'No version specified. Consider adding a version following semver (e.g., "1.0.0")',
      })
    }

    // Warn if no description
    if (!manifest.description) {
      warnings.push({
        path: 'description',
        message:
          'No description provided. Adding a description helps users understand what your plugin does',
      })
    }

    // Warn if no author
    if (!manifest.author) {
      warnings.push({
        path: 'author',
        message:
          'No author information provided. Consider adding author details for plugin attribution',
      })
    }
  }

  return {
    success: errors.length === 0,
    errors,
    warnings,
    filePath: absolutePath,
    fileType: 'plugin',
  }
}

/**
 * Validate a marketplace manifest file (marketplace.json)
 */
export async function validateMarketplaceManifest(
  filePath: string,
): Promise<ValidationResult> {
  const errors: ValidationError[] = []
  const warnings: ValidationWarning[] = []
  const absolutePath = path.resolve(filePath)

  // Read file content — handle ENOENT / EISDIR / permission errors directly
  let content: string
  let marketplaceRoot = getManifestRoot(absolutePath)
  try {
    const resolved = await resolveManifestFileForValidation(
      absolutePath,
      'marketplace manifest',
    )
    marketplaceRoot = resolved.root
    content = await readCanonicalPluginTextFile(
      marketplaceRoot,
      resolved.filePath,
      'marketplace manifest',
    )
  } catch (error: unknown) {
    const code =
      error instanceof PluginPathSecurityError && error.reason === 'path-missing'
        ? 'ENOENT'
        : getErrnoCode(error)
    let message: string
    if (code === 'ENOENT') {
      message = `File not found: ${absolutePath}`
    } else if (code === 'EISDIR') {
      message = `Path is not a file: ${absolutePath}`
    } else {
      message = `Failed to read file: ${errorMessage(error)}`
    }
    return {
      success: false,
      errors: [{ path: 'file', message, code }],
      warnings: [],
      filePath: absolutePath,
      fileType: 'marketplace',
    }
  }

  let parsed: unknown
  try {
    parsed = jsonParse(content)
  } catch (error) {
    return {
      success: false,
      errors: [
        {
          path: 'json',
          message: `Invalid JSON syntax: ${errorMessage(error)}`,
        },
      ],
      warnings: [],
      filePath: absolutePath,
      fileType: 'marketplace',
    }
  }

  // Check for path traversal in plugin sources before schema validation
  // This ensures we catch security issues even if schema validation fails
  if (parsed && typeof parsed === 'object') {
    const obj = parsed as Record<string, unknown>

    if (Array.isArray(obj.plugins)) {
      obj.plugins.forEach((plugin: unknown, i: number) => {
        if (plugin && typeof plugin === 'object' && 'source' in plugin) {
          const source = (plugin as { source: unknown }).source
          // Check string sources (relative paths)
          if (typeof source === 'string') {
            checkPathTraversal(
              source,
              `plugins[${i}].source`,
              errors,
              marketplaceSourceHint(source),
            )
          }
          // Check object-source .path (git-subdir: subdirectory within the
          // remote repo, sparse-cloned). '..' here is a genuine traversal attempt
          // within the remote repo tree, not a marketplace-root misunderstanding —
          // keep the security framing (no marketplaceSourceHint). See #20895 review.
          if (
            source &&
            typeof source === 'object' &&
            'path' in source &&
            typeof (source as { path: unknown }).path === 'string'
          ) {
            checkPathTraversal(
              (source as { path: string }).path,
              `plugins[${i}].source.path`,
              errors,
            )
          }
        }
      })
    }
  }

  // Validate against schema.
  // The base schemas are lenient (strip unknown keys) for runtime resilience,
  // but this is a developer tool — authors want typo feedback. We rebuild the
  // schema with .strict() here. Note .strict() on the outer object does NOT
  // propagate into z.array() elements, so we also override the plugins array
  // with strict entries to catch typos inside individual plugin entries too.
  const strictMarketplaceSchema = PluginMarketplaceSchema()
    .extend({
      plugins: z.array(PluginMarketplaceEntrySchema().strict()),
    })
    .strict()
  const result = strictMarketplaceSchema.safeParse(parsed)

  if (!result.success) {
    errors.push(...formatZodErrors(result.error))
  }

  // Check for common issues and add warnings
  if (result.success) {
    const marketplace = result.data

    // Warn if no plugins
    if (!marketplace.plugins || marketplace.plugins.length === 0) {
      warnings.push({
        path: 'plugins',
        message: 'Marketplace has no plugins defined',
      })
    }

    // Check each plugin entry
    if (marketplace.plugins) {
      marketplace.plugins.forEach((plugin, i) => {
        // Check for duplicate plugin names
        const duplicates = marketplace.plugins.filter(
          p => p.name === plugin.name,
        )
        if (duplicates.length > 1) {
          errors.push({
            path: `plugins[${i}].name`,
            message: `Duplicate plugin name "${plugin.name}" found in marketplace`,
          })
        }
      })

      // Version-mismatch check: for local-source entries that declare a
      // version, compare against the plugin's own plugin.json. At install
      // time, calculatePluginVersion (pluginVersioning.ts) prefers the
      // manifest version and silently ignores the entry version — so a
      // stale entry.version is invisible user confusion (marketplace UI
      // shows one version, /status shows another after install).
      // Only local sources: remote sources would need cloning to check.
      for (const [i, entry] of marketplace.plugins.entries()) {
        if (typeof entry.source === 'string') {
          try {
            const pluginRoot = await resolvePluginComponentPath(
              marketplaceRoot,
              entry.source,
              { component: `marketplace plugin source ${entry.name}` },
            )
            await validateManifestComponentPaths(
              pluginRoot,
              entry as unknown as Record<string, unknown>,
              errors,
              `plugins[${i}]`,
            )
          } catch (error) {
            errors.push({
              path: `plugins[${i}].source`,
              message:
                error instanceof PluginPathSecurityError
                  ? error.message
                  : `Plugin source cannot be validated: ${errorMessage(error)}`,
              code:
                error instanceof PluginPathSecurityError
                  ? error.reason
                  : undefined,
            })
          }
        }

        if (
          !entry.version ||
          typeof entry.source !== 'string' ||
          !entry.source.startsWith('./')
        ) {
          continue
        }
        let manifestVersion: string | undefined
        try {
          const pluginRoot = await resolvePluginComponentPath(
            marketplaceRoot,
            entry.source,
            { component: `marketplace plugin source ${entry.name}` },
          )
          const pluginJsonPath = await resolvePluginComponentPath(
            pluginRoot,
            '.crabcode-plugin/plugin.json',
            { component: 'plugin manifest' },
          )
          const raw = await readCanonicalPluginTextFile(
            pluginRoot,
            pluginJsonPath,
            'plugin manifest',
          )
          const parsed = jsonParse(raw) as { version?: unknown }
          if (typeof parsed.version === 'string') {
            manifestVersion = parsed.version
          }
        } catch {
          // Missing/unreadable plugin.json is someone else's error to report
          continue
        }
        if (manifestVersion && manifestVersion !== entry.version) {
          warnings.push({
            path: `plugins[${i}].version`,
            message:
              `Entry declares version "${entry.version}" but ${entry.source}/.crabcode-plugin/plugin.json says "${manifestVersion}". ` +
              `At install time, plugin.json wins (calculatePluginVersion precedence) — the entry version is silently ignored. ` +
              `Update this entry to "${manifestVersion}" to match.`,
          })
        }
      }
    }

    // Warn if no description in metadata
    if (!marketplace.metadata?.description) {
      warnings.push({
        path: 'metadata.description',
        message:
          'No marketplace description provided. Adding a description helps users understand what this marketplace offers',
      })
    }
  }

  return {
    success: errors.length === 0,
    errors,
    warnings,
    filePath: absolutePath,
    fileType: 'marketplace',
  }
}
/**
 * Validate the YAML frontmatter in a plugin component markdown file.
 *
 * The runtime loader (parseFrontmatter) silently drops unparseable YAML to a
 * debug log and returns an empty object. That's the right resilience choice
 * for the load path, but authors running `crabcode plugin validate` want a hard
 * signal. This re-parses the frontmatter block and surfaces what the loader
 * would silently swallow.
 */
function validateComponentFile(
  filePath: string,
  content: string,
  fileType: 'skill' | 'agent' | 'command',
): ValidationResult {
  const errors: ValidationError[] = []
  const warnings: ValidationWarning[] = []

  const match = content.match(FRONTMATTER_REGEX)
  if (!match) {
    warnings.push({
      path: 'frontmatter',
      message:
        'No frontmatter block found. Add YAML frontmatter between --- delimiters ' +
        'at the top of the file to set description and other metadata.',
    })
    return { success: true, errors, warnings, filePath, fileType }
  }

  const frontmatterText = match[1] || ''
  let parsed: unknown
  try {
    parsed = parseYaml(frontmatterText)
  } catch (e) {
    errors.push({
      path: 'frontmatter',
      message:
        `YAML frontmatter failed to parse: ${errorMessage(e)}. ` +
        `At runtime this ${fileType} loads with empty metadata (all frontmatter ` +
        `fields silently dropped).`,
    })
    return { success: false, errors, warnings, filePath, fileType }
  }

  if (parsed === null || typeof parsed !== 'object' || Array.isArray(parsed)) {
    errors.push({
      path: 'frontmatter',
      message:
        'Frontmatter must be a YAML mapping (key: value pairs), got ' +
        `${Array.isArray(parsed) ? 'an array' : parsed === null ? 'null' : typeof parsed}.`,
    })
    return { success: false, errors, warnings, filePath, fileType }
  }

  const fm = parsed as Record<string, unknown>

  // description: must be scalar. coerceDescriptionToString logs+drops arrays/objects at runtime.
  if (fm.description !== undefined) {
    const d = fm.description
    if (
      typeof d !== 'string' &&
      typeof d !== 'number' &&
      typeof d !== 'boolean' &&
      d !== null
    ) {
      errors.push({
        path: 'description',
        message:
          `description must be a string, got ${Array.isArray(d) ? 'array' : typeof d}. ` +
          `At runtime this value is dropped.`,
      })
    }
  } else {
    warnings.push({
      path: 'description',
      message:
        `No description in frontmatter. A description helps users and CrabCode ` +
        `understand when to use this ${fileType}.`,
    })
  }

  // name: if present, must be a string (skills/commands use it as displayName;
  // plugin agents use it as the agentType stem — non-strings would stringify to garbage)
  if (
    fm.name !== undefined &&
    fm.name !== null &&
    typeof fm.name !== 'string'
  ) {
    errors.push({
      path: 'name',
      message: `name must be a string, got ${typeof fm.name}.`,
    })
  }

  // allowed-tools: string or array of strings
  const at = fm['allowed-tools']
  if (at !== undefined && at !== null) {
    if (typeof at !== 'string' && !Array.isArray(at)) {
      errors.push({
        path: 'allowed-tools',
        message: `allowed-tools must be a string or array of strings, got ${typeof at}.`,
      })
    } else if (Array.isArray(at) && at.some(t => typeof t !== 'string')) {
      errors.push({
        path: 'allowed-tools',
        message: 'allowed-tools array must contain only strings.',
      })
    }
  }

  // shell: 'bash' | 'powershell' (controls !`cmd` block routing)
  const sh = fm.shell
  if (sh !== undefined && sh !== null) {
    if (typeof sh !== 'string') {
      errors.push({
        path: 'shell',
        message: `shell must be a string, got ${typeof sh}.`,
      })
    } else {
      // Normalize to match parseShellFrontmatter() runtime behavior —
      // `shell: PowerShell` should not fail validation but work at runtime.
      const normalized = sh.trim().toLowerCase()
      if (normalized !== 'bash' && normalized !== 'powershell') {
        errors.push({
          path: 'shell',
          message: `shell must be 'bash' or 'powershell', got '${sh}'.`,
        })
      }
    }
  }

  return { success: errors.length === 0, errors, warnings, filePath, fileType }
}

/**
 * Validate a plugin's hooks.json file. Unlike frontmatter, this one HARD-ERRORS
 * at runtime (pluginLoader uses .parse() not .safeParse()) — a bad hooks.json
 * breaks the whole plugin. Surfacing it here is essential.
 */
async function validateHooksJson(
  pluginRoot: string,
  filePath: string,
): Promise<ValidationResult> {
  let content: string
  try {
    const safeFilePath = await resolveInternalPluginPath(
      pluginRoot,
      filePath,
      { component: 'hooks config' },
    )
    content = await readCanonicalPluginTextFile(
      pluginRoot,
      safeFilePath,
      'hooks config',
    )
  } catch (e: unknown) {
    // Native ENOENT is a post-resolution disappearance race. The central
    // resolver's path-missing classification is the ordinary optional case.
    if (
      isENOENT(e) ||
      (e instanceof PluginPathSecurityError && e.reason === 'path-missing')
    ) {
      return {
        success: true,
        errors: [],
        warnings: [],
        filePath,
        fileType: 'hooks',
      }
    }
    return {
      success: false,
      errors: [
        {
          path: 'file',
          message:
            e instanceof PluginPathSecurityError
              ? e.message
              : `Failed to read file: ${errorMessage(e)}`,
          code:
            e instanceof PluginPathSecurityError ? e.reason : undefined,
        },
      ],
      warnings: [],
      filePath,
      fileType: 'hooks',
    }
  }

  let parsed: unknown
  try {
    parsed = jsonParse(content)
  } catch (e) {
    return {
      success: false,
      errors: [
        {
          path: 'json',
          message:
            `Invalid JSON syntax: ${errorMessage(e)}. ` +
            `At runtime this breaks the entire plugin load.`,
        },
      ],
      warnings: [],
      filePath,
      fileType: 'hooks',
    }
  }

  const result = PluginHooksSchema().safeParse(parsed)
  if (!result.success) {
    return {
      success: false,
      errors: formatZodErrors(result.error),
      warnings: [],
      filePath,
      fileType: 'hooks',
    }
  }

  return {
    success: true,
    errors: [],
    warnings: [],
    filePath,
    fileType: 'hooks',
  }
}

/**
 * Recursively collect .md files under a directory. Uses withFileTypes to
 * avoid a stat per entry. Returns absolute paths so error messages stay
 * readable.
 */
async function collectMarkdown(
  dir: string,
  isSkillsDir: boolean,
  pluginRoot: string,
): Promise<string[]> {
  let safeDir: string
  let entries: Dirent[]
  try {
    safeDir = await resolveInternalPluginPath(pluginRoot, dir, {
      component: 'plugin component directory',
    })
    entries = await readdir(safeDir, { withFileTypes: true })
    await revalidatePluginPath(
      pluginRoot,
      safeDir,
      'plugin component directory',
    )
  } catch (e: unknown) {
    const code = getErrnoCode(e)
    if (
      code === 'ENOENT' ||
      code === 'ENOTDIR' ||
      (e instanceof PluginPathSecurityError && e.reason === 'path-missing')
    ) {
      return []
    }
    throw e
  }

  // Skills use <name>/SKILL.md — only descend one level, only collect SKILL.md.
  // Matches the runtime loader: single .md files in skills/ are NOT loaded,
  // and subdirectories of a skill dir aren't scanned. Paths are speculative
  // (the subdir may lack SKILL.md); the caller handles ENOENT.
  if (isSkillsDir) {
    const skills: string[] = []
    for (const entry of entries) {
      if (!entry.isDirectory() && !entry.isSymbolicLink()) continue
      const candidate = path.join(safeDir, entry.name)
      const safeEntry = await resolveInternalPluginPath(pluginRoot, candidate, {
        component: 'skill directory',
      })
      if (!(await stat(safeEntry)).isDirectory()) continue
      const skillPath = await resolveInternalPluginPath(
        pluginRoot,
        path.join(safeEntry, 'SKILL.md'),
        { mustExist: false, component: 'skill markdown' },
      )
      skills.push(skillPath)
    }
    return skills
  }

  // Commands/agents: recurse and collect all .md files.
  const out: string[] = []
  for (const entry of entries) {
    if (
      !entry.isDirectory() &&
      !entry.isFile() &&
      !entry.isSymbolicLink()
    ) {
      continue
    }
    const candidate = path.join(safeDir, entry.name)
    const full = await resolveInternalPluginPath(pluginRoot, candidate, {
      component: 'plugin markdown entry',
    })
    const entryStat = await stat(full)
    if (entryStat.isDirectory()) {
      out.push(...(await collectMarkdown(full, false, pluginRoot)))
    } else if (entryStat.isFile() && entry.name.toLowerCase().endsWith('.md')) {
      out.push(full)
    }
  }
  return out
}

/**
 * Validate the content files inside a plugin directory — skills, agents,
 * commands, and hooks.json. Scans the default component directories (the
 * manifest can declare custom paths but the default layout covers the vast
 * majority of plugins; this is a linter, not a loader).
 *
 * Returns one ValidationResult per file that has errors or warnings. A clean
 * plugin returns an empty array.
 */
export async function validatePluginContents(
  pluginDir: string,
): Promise<ValidationResult[]> {
  const results: ValidationResult[] = []
  const pluginRoot = await resolveInternalPluginPath(pluginDir, pluginDir, {
    component: 'plugin root',
  })

  const dirs: Array<['skill' | 'agent' | 'command', string]> = [
    ['skill', path.join(pluginRoot, 'skills')],
    ['agent', path.join(pluginRoot, 'agents')],
    ['command', path.join(pluginRoot, 'commands')],
  ]

  for (const [fileType, dir] of dirs) {
    let files: string[]
    try {
      files = await collectMarkdown(
        dir,
        fileType === 'skill',
        pluginRoot,
      )
    } catch (error) {
      results.push({
        success: false,
        errors: [
          {
            path: 'file',
            message: `Unsafe plugin ${fileType} path: ${errorMessage(error)}`,
            code:
              error instanceof PluginPathSecurityError
                ? error.reason
                : undefined,
          },
        ],
        warnings: [],
        filePath: dir,
        fileType,
      })
      continue
    }
    for (const filePath of files) {
      let content: string
      try {
        content = await readCanonicalPluginTextFile(
          pluginRoot,
          filePath,
          `${fileType} markdown`,
        )
      } catch (e: unknown) {
        // ENOENT is expected for speculative skill paths (subdirs without SKILL.md)
        if (isENOENT(e)) continue
        results.push({
          success: false,
          errors: [
            { path: 'file', message: `Failed to read: ${errorMessage(e)}` },
          ],
          warnings: [],
          filePath,
          fileType,
        })
        continue
      }
      const r = validateComponentFile(filePath, content, fileType)
      if (r.errors.length > 0 || r.warnings.length > 0) {
        results.push(r)
      }
    }
  }

  const hooksResult = await validateHooksJson(
    pluginRoot,
    path.join(pluginRoot, 'hooks', 'hooks.json'),
  )
  if (hooksResult.errors.length > 0 || hooksResult.warnings.length > 0) {
    results.push(hooksResult)
  }

  return results
}

/**
 * Validate a manifest file or directory (auto-detects type)
 */
export async function validateManifest(
  filePath: string,
): Promise<ValidationResult> {
  const absolutePath = path.resolve(filePath)
  const manifestType = detectManifestType(filePath)

  // Stat path to check if it's a directory — handle ENOENT inline
  let stats: Stats | null = null
  try {
    stats = await stat(absolutePath)
  } catch (e: unknown) {
    try {
      await resolvePluginComponentPath(absolutePath, '.', {
        component: 'plugin validation root',
      })
    } catch (rootError) {
      if (rootError instanceof PluginPathSecurityError) {
        return {
          success: false,
          errors: [
            {
              path: 'path',
              message: rootError.message,
              code: rootError.reason,
            },
          ],
          warnings: [],
          filePath: absolutePath,
          fileType: manifestType === 'marketplace' ? 'marketplace' : 'plugin',
        }
      }
    }
    throw e
  }

  if (stats?.isDirectory()) {
    // Look for manifest files in .crabcode-plugin directory
    // Prefer marketplace.json over plugin.json
    const marketplacePath = path.join(
      absolutePath,
      '.crabcode-plugin',
      'marketplace.json',
    )
    const marketplaceResult = await validateMarketplaceManifest(marketplacePath)
    // Only fall through if the marketplace file was not found (ENOENT)
    if (marketplaceResult.errors[0]?.code !== 'ENOENT') {
      return marketplaceResult
    }

    const pluginPath = path.join(absolutePath, '.crabcode-plugin', 'plugin.json')
    const pluginResult = await validatePluginManifest(pluginPath)
    if (pluginResult.errors[0]?.code !== 'ENOENT') {
      return pluginResult
    }

    return {
      success: false,
      errors: [
        {
          path: 'directory',
          message: `No manifest found in directory. Expected .crabcode-plugin/marketplace.json or .crabcode-plugin/plugin.json`,
        },
      ],
      warnings: [],
      filePath: absolutePath,
      fileType: 'plugin',
    }
  }

  switch (manifestType) {
    case 'plugin':
      return validatePluginManifest(filePath)
    case 'marketplace':
      return validateMarketplaceManifest(filePath)
    case 'unknown': {
      // Try to parse and guess based on content
      try {
        const content = await readFile(absolutePath, { encoding: 'utf-8' })
        const parsed = jsonParse(content) as Record<string, unknown>

        // Heuristic: if it has a "plugins" array, it's probably a marketplace
        if (Array.isArray(parsed.plugins)) {
          return validateMarketplaceManifest(filePath)
        }
      } catch (e: unknown) {
        const code = getErrnoCode(e)
        if (code === 'ENOENT') {
          return {
            success: false,
            errors: [
              {
                path: 'file',
                message: `File not found: ${absolutePath}`,
              },
            ],
            warnings: [],
            filePath: absolutePath,
            fileType: 'plugin', // Default to plugin for error reporting
          }
        }
        // Fall through to default validation for other errors (e.g., JSON parse)
      }

      // Default: validate as plugin manifest
      return validatePluginManifest(filePath)
    }
  }
}
