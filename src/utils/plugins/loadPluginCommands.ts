import memoize from 'lodash-es/memoize.js'
import { basename, dirname, join } from 'path'
import { getInlinePlugins, getSessionId } from '../../bootstrap/state.js'
import type { Command, SkillInterface } from '../../types/command.js'
import { getPluginErrorMessage } from '../../types/plugin.js'
import {
  parseArgumentNames,
  substituteArguments,
} from '../argumentSubstitution.js'
import { logForDebugging } from '../debug.js'
import { EFFORT_LEVELS, parseEffortValue } from '../effort.js'
import { isBareMode } from '../envUtils.js'
import { isENOENT } from '../errors.js'
import {
  coerceDescriptionToString,
  type FrontmatterData,
  parseBooleanFrontmatter,
  parseFrontmatter,
  parseShellFrontmatter,
} from '../frontmatterParser.js'
import { getFsImplementation, isDuplicatePath } from '../fsOperations.js'
import {
  extractDescriptionFromMarkdown,
  parseSlashCommandToolsFromFrontmatter,
} from '../markdownConfigLoader.js'
import { parseUserSpecifiedModel } from '../model/model.js'
import { executeShellCommandsInPrompt } from '../promptShellExecution.js'
import { loadAllPluginsCacheOnly } from './pluginLoader.js'
import {
  loadPluginOptions,
  substitutePluginVariables,
  substituteUserConfigInContent,
} from './pluginOptionsStorage.js'
import type { CommandMetadata, PluginManifest } from './schemas.js'
import {
  readCanonicalPluginTextFile,
  revalidatePluginPath,
  resolveInternalPluginPath,
  resolvePluginComponentPath,
} from './pluginPathSecurity.js'
import { walkPluginMarkdown } from './walkPluginMarkdown.js'

// Similar to MarkdownFile but for plugin sources
type PluginMarkdownFile = {
  filePath: string
  baseDir: string
  frontmatter: FrontmatterData
  content: string
}

// Configuration for loading commands or skills
type LoadConfig = {
  isSkillMode: boolean // true when loading from skills/ directory
}

function firstDeclaredFrontmatterValue(
  frontmatter: FrontmatterData,
  keys: readonly string[],
): unknown {
  for (const key of keys) {
    if (Object.prototype.hasOwnProperty.call(frontmatter, key)) {
      return frontmatter[key]
    }
  }
  return undefined
}

function optionalTrimmedString(value: unknown): string | undefined {
  if (typeof value !== 'string') return undefined
  const trimmed = value.trim()
  return trimmed.length > 0 ? trimmed : undefined
}

/**
 * Parse display-only skill frontmatter. Hyphenated keys are the canonical
 * source; camelCase and snake_case aliases keep existing third-party skills
 * compatible without allowing aliases to override an explicitly-declared
 * canonical value.
 */
function skillInterfaceFromFrontmatter(
  frontmatter: FrontmatterData,
  displayName: string | undefined,
): SkillInterface | undefined {
  // `display-name:` is the only way a skill file declares a display identity
  // distinct from its invocation token. The `displayName` argument falls back
  // to `name:`, which is that token — so a file that declares nothing keeps
  // echoing the token, and consumers (worker skill-list) can tell the two
  // apart and let a catalog localization win over a bare echo.
  const declaredDisplayName = optionalTrimmedString(
    firstDeclaredFrontmatterValue(frontmatter, [
      'display-name',
      'displayName',
      'display_name',
    ]),
  )
  const effectiveDisplayName = declaredDisplayName ?? displayName
  const shortDescription = optionalTrimmedString(
    firstDeclaredFrontmatterValue(frontmatter, [
      'short-description',
      'shortDescription',
      'short_description',
    ]),
  )
  const result: SkillInterface = {
    ...(effectiveDisplayName !== undefined
      ? { displayName: effectiveDisplayName }
      : {}),
    ...(shortDescription !== undefined ? { shortDescription } : {}),
  }
  return Object.keys(result).length > 0 ? result : undefined
}

/**
 * Check if a file path is a skill file (SKILL.md)
 */
function isSkillFile(filePath: string): boolean {
  return /^skill\.md$/i.test(basename(filePath))
}

/**
 * Get command name from file path, handling both regular files and skills
 */
function getCommandNameFromFile(
  filePath: string,
  baseDir: string,
  pluginName: string,
): string {
  const isSkill = isSkillFile(filePath)

  if (isSkill) {
    // For skills, use the parent directory name
    const skillDirectory = dirname(filePath)
    const parentOfSkillDir = dirname(skillDirectory)
    const commandBaseName = basename(skillDirectory)

    // Build namespace from parent of skill directory
    const relativePath = parentOfSkillDir.startsWith(baseDir)
      ? parentOfSkillDir.slice(baseDir.length).replace(/^\//, '')
      : ''
    const namespace = relativePath ? relativePath.split('/').join(':') : ''

    return namespace
      ? `${pluginName}:${namespace}:${commandBaseName}`
      : `${pluginName}:${commandBaseName}`
  } else {
    // For regular files, use filename without .md
    const fileDirectory = dirname(filePath)
    const commandBaseName = basename(filePath).replace(/\.md$/, '')

    // Build namespace from file directory
    const relativePath = fileDirectory.startsWith(baseDir)
      ? fileDirectory.slice(baseDir.length).replace(/^\//, '')
      : ''
    const namespace = relativePath ? relativePath.split('/').join(':') : ''

    return namespace
      ? `${pluginName}:${namespace}:${commandBaseName}`
      : `${pluginName}:${commandBaseName}`
  }
}

/**
 * Recursively collects all markdown files from a directory
 */
async function collectMarkdownFiles(
  dirPath: string,
  baseDir: string,
  pluginPath: string,
  loadedPaths: Set<string>,
): Promise<PluginMarkdownFile[]> {
  const files: PluginMarkdownFile[] = []
  const fs = getFsImplementation()

  await walkPluginMarkdown(
    dirPath,
    async fullPath => {
      if (isDuplicatePath(fs, fullPath, loadedPaths)) return
      const content = await readCanonicalPluginTextFile(
        pluginPath,
        fullPath,
        'command markdown',
      )
      const { frontmatter, content: markdownContent } = parseFrontmatter(
        content,
        fullPath,
      )
      files.push({
        filePath: fullPath,
        baseDir,
        frontmatter,
        content: markdownContent,
      })
    },
    {
      stopAtSkillDir: true,
      logLabel: 'commands',
      pluginRoot: pluginPath,
    },
  )

  return files
}

/**
 * Transforms plugin markdown files to handle skill directories
 */
function transformPluginSkillFiles(
  files: PluginMarkdownFile[],
): PluginMarkdownFile[] {
  const filesByDir = new Map<string, PluginMarkdownFile[]>()

  for (const file of files) {
    const dir = dirname(file.filePath)
    const dirFiles = filesByDir.get(dir) ?? []
    dirFiles.push(file)
    filesByDir.set(dir, dirFiles)
  }

  const result: PluginMarkdownFile[] = []

  for (const [dir, dirFiles] of filesByDir) {
    const skillFiles = dirFiles.filter(f => isSkillFile(f.filePath))
    if (skillFiles.length > 0) {
      // Use the first skill file if multiple exist
      const skillFile = skillFiles[0]!
      if (skillFiles.length > 1) {
        logForDebugging(
          `Multiple skill files found in ${dir}, using ${basename(skillFile.filePath)}`,
        )
      }
      // Directory has a skill - only include the skill file
      result.push(skillFile)
    } else {
      result.push(...dirFiles)
    }
  }

  return result
}

async function loadCommandsFromDirectory(
  commandsPath: string,
  pluginName: string,
  sourceName: string,
  pluginManifest: PluginManifest,
  pluginPath: string,
  config: LoadConfig = { isSkillMode: false },
  loadedPaths: Set<string> = new Set(),
): Promise<Command[]> {
  // Collect all markdown files
  const markdownFiles = await collectMarkdownFiles(
    commandsPath,
    commandsPath,
    pluginPath,
    loadedPaths,
  )

  // Apply skill transformation
  const processedFiles = transformPluginSkillFiles(markdownFiles)

  // Convert to commands
  const commands: Command[] = []
  for (const file of processedFiles) {
    const commandName = getCommandNameFromFile(
      file.filePath,
      file.baseDir,
      pluginName,
    )

    const command = createPluginCommand(
      commandName,
      file,
      sourceName,
      pluginManifest,
      pluginPath,
      isSkillFile(file.filePath),
      config,
    )

    if (command) {
      commands.push(command)
    }
  }

  return commands
}

/**
 * Create a Command from a plugin markdown file
 */
function createPluginCommand(
  commandName: string,
  file: PluginMarkdownFile,
  sourceName: string,
  pluginManifest: PluginManifest,
  pluginPath: string,
  isSkill: boolean,
  config: LoadConfig = { isSkillMode: false },
): Command | null {
  try {
    const { frontmatter, content } = file

    const validatedDescription = coerceDescriptionToString(
      frontmatter.description,
      commandName,
    )
    const description =
      validatedDescription ??
      extractDescriptionFromMarkdown(
        content,
        isSkill ? 'Plugin skill' : 'Plugin command',
      )

    // Substitute ${CRABCODE_PLUGIN_ROOT} in allowed-tools before parsing
    const rawAllowedTools = frontmatter['allowed-tools']
    const substitutedAllowedTools =
      typeof rawAllowedTools === 'string'
        ? substitutePluginVariables(rawAllowedTools, {
            path: pluginPath,
            source: sourceName,
          })
        : Array.isArray(rawAllowedTools)
          ? rawAllowedTools.map(tool =>
              typeof tool === 'string'
                ? substitutePluginVariables(tool, {
                    path: pluginPath,
                    source: sourceName,
                  })
                : tool,
            )
          : rawAllowedTools
    const allowedTools = parseSlashCommandToolsFromFrontmatter(
      substitutedAllowedTools,
    )

    const argumentHint = frontmatter['argument-hint'] as string | undefined
    const argumentNames = parseArgumentNames(
      frontmatter.arguments as string | string[] | undefined,
    )
    const whenToUse = frontmatter.when_to_use as string | undefined
    const version = frontmatter.version as string | undefined
    const displayName = optionalTrimmedString(frontmatter.name)
    const isSkillCommand = isSkill || config.isSkillMode
    const skillInterface = isSkillCommand
      ? skillInterfaceFromFrontmatter(frontmatter, displayName)
      : undefined

    // Handle model configuration, resolving semantic aliases ('best'/'planmode')
    // or full SDK model ids via parseUserSpecifiedModel.
    const model =
      frontmatter.model === 'inherit'
        ? undefined
        : frontmatter.model
          ? parseUserSpecifiedModel(frontmatter.model as string)
          : undefined

    const effortRaw = frontmatter['effort']
    const effort =
      effortRaw !== undefined ? parseEffortValue(effortRaw) : undefined
    if (effortRaw !== undefined && effort === undefined) {
      logForDebugging(
        `Plugin command ${commandName} has invalid effort '${effortRaw}'. Valid options: ${EFFORT_LEVELS.join(', ')} or an integer`,
      )
    }

    const disableModelInvocation = parseBooleanFrontmatter(
      frontmatter['disable-model-invocation'],
    )

    const userInvocableValue = frontmatter['user-invocable']
    const userInvocable =
      userInvocableValue === undefined
        ? true
        : parseBooleanFrontmatter(userInvocableValue)

    const shell = parseShellFrontmatter(frontmatter.shell, commandName)

    return {
      type: 'prompt',
      name: commandName,
      description,
      hasUserSpecifiedDescription: validatedDescription !== null,
      allowedTools,
      argumentHint,
      argNames: argumentNames.length > 0 ? argumentNames : undefined,
      whenToUse,
      version,
      model,
      effort,
      disableModelInvocation,
      userInvocable,
      contentLength: content.length,
      source: 'plugin' as const,
      loadedFrom: isSkillCommand ? 'plugin' : undefined,
      // 插件贡献的 skill 必须带 `skillRoot`（= SKILL.md 所在目录），否则
      // skill catalog projection 会在 `cmd.skillRoot` 为空时丢弃它们，而
      // path 合成兜底只对 `sourceKind === 'builtin'` 生效（plugin 不适用）。
      // 文件型 skill（loadSkillsDir.ts:343）与 bundled skill（bundledSkills.ts:92）
      // 本来就设 skillRoot —— 插件 skill 是唯一漏设的来源，导致 0 个插件 skill 进
      // skills/list。仅对 skill
      // 设置（命令不需要），与上方 `loadedFrom` 的 isSkill 门控一致。`file.baseDir`
      // 在两个 createPluginCommand 调用点均 = `dirname(SKILL.md)` = skill 自身目录。
      skillRoot: isSkillCommand ? file.baseDir : undefined,
      pluginInfo: {
        pluginManifest,
        repository: sourceName,
      },
      isHidden: !userInvocable,
      progressMessage: isSkillCommand ? 'loading' : 'running',
      skillInterface,
      userFacingName(): string {
        return displayName || commandName
      },
      async getPromptForCommand(args, context) {
        // For skills from skills/ directory, include base directory
        let finalContent = config.isSkillMode
          ? `Base directory for this skill: ${dirname(file.filePath)}\n\n${content}`
          : content

        finalContent = substituteArguments(
          finalContent,
          args,
          true,
          argumentNames,
        )

        // Replace ${CRABCODE_PLUGIN_ROOT} and ${CRABCODE_PLUGIN_DATA} with their paths
        finalContent = substitutePluginVariables(finalContent, {
          path: pluginPath,
          source: sourceName,
        })

        // Replace ${user_config.X} with saved option values. Sensitive keys
        // resolve to a descriptive placeholder instead — skill content goes to
        // the model prompt and we don't put secrets there.
        if (pluginManifest.userConfig) {
          finalContent = substituteUserConfigInContent(
            finalContent,
            loadPluginOptions(sourceName),
            pluginManifest.userConfig,
          )
        }

        // Replace ${CRABCODE_SKILL_DIR} with this specific skill's directory.
        // Distinct from ${CRABCODE_PLUGIN_ROOT}: a plugin can contain multiple
        // skills, so CRABCODE_PLUGIN_ROOT points to the plugin root while
        // CRABCODE_SKILL_DIR points to the individual skill's subdirectory.
        if (config.isSkillMode) {
          const rawSkillDir = dirname(file.filePath)
          const skillDir =
            process.platform === 'win32'
              ? rawSkillDir.replace(/\\/g, '/')
              : rawSkillDir
          finalContent = finalContent.replace(
            /\$\{CRABCODE_SKILL_DIR\}/g,
            skillDir,
          )
        }

        // Replace ${CRABCODE_SESSION_ID} with the current session ID
        finalContent = finalContent.replace(
          /\$\{CRABCODE_SESSION_ID\}/g,
          getSessionId(),
        )

        finalContent = await executeShellCommandsInPrompt(
          finalContent,
          {
            ...context,
            getAppState() {
              const appState = context.getAppState()
              return {
                ...appState,
                toolPermissionContext: {
                  ...appState.toolPermissionContext,
                  alwaysAllowRules: {
                    ...appState.toolPermissionContext.alwaysAllowRules,
                    command: allowedTools,
                  },
                },
              }
            },
          },
          `/${commandName}`,
          shell,
        )

        return [{ type: 'text', text: finalContent }]
      },
    } satisfies Command
  } catch (error) {
    logForDebugging(
      `Failed to create command from ${file.filePath}: ${error}`,
      {
        level: 'error',
      },
    )
    return null
  }
}

export const getPluginCommands = memoize(async (): Promise<Command[]> => {
  // --bare: skip marketplace plugin auto-load. Explicit --plugin-dir still
  // works — getInlinePlugins() is set by main.tsx from --plugin-dir.
  // loadAllPluginsCacheOnly already short-circuits to inline-only when
  // inlinePlugins.length > 0.
  if (isBareMode() && getInlinePlugins().length === 0) {
    return []
  }
  // Only load commands from enabled plugins
  const { enabled, errors } = await loadAllPluginsCacheOnly()

  if (errors.length > 0) {
    logForDebugging(
      `Plugin loading errors: ${errors.map(e => getPluginErrorMessage(e)).join(', ')}`,
    )
  }

  // Process plugins in parallel; each plugin has its own loadedPaths scope
  const perPluginCommands = await Promise.all(
    enabled.map(async (plugin): Promise<Command[]> => {
      // Track loaded file paths to prevent duplicates within this plugin
      const loadedPaths = new Set<string>()
      const pluginCommands: Command[] = []

      // Load commands from default commands directory
      if (plugin.commandsPath) {
        try {
          const commands = await loadCommandsFromDirectory(
            plugin.commandsPath,
            plugin.name,
            plugin.source,
            plugin.manifest,
            plugin.path,
            { isSkillMode: false },
            loadedPaths,
          )
          pluginCommands.push(...commands)

          if (commands.length > 0) {
            logForDebugging(
              `Loaded ${commands.length} commands from plugin ${plugin.name} default directory`,
            )
          }
        } catch (error) {
          logForDebugging(
            `Failed to load commands from plugin ${plugin.name} default directory: ${error}`,
            { level: 'error' },
          )
        }
      }

      // Load commands from additional paths specified in manifest
      if (plugin.commandsPaths) {
        logForDebugging(
          `Plugin ${plugin.name} has commandsPaths: ${plugin.commandsPaths.join(', ')}`,
        )
        // Process all commandsPaths in parallel. isDuplicatePath is synchronous
        // (check-and-add), so concurrent access to loadedPaths is safe.
        const pathResults = await Promise.all(
          plugin.commandsPaths.map(async (commandPath): Promise<Command[]> => {
            try {
              commandPath = await resolveInternalPluginPath(
                plugin.path,
                commandPath,
                { component: 'command path' },
              )
              const fs = getFsImplementation()
              const stats = await fs.stat(commandPath)
              logForDebugging(
                `Checking commandPath ${commandPath} - isDirectory: ${stats.isDirectory()}, isFile: ${stats.isFile()}`,
              )

              if (stats.isDirectory()) {
                // Load all .md files and skill directories from directory
                const commands = await loadCommandsFromDirectory(
                  commandPath,
                  plugin.name,
                  plugin.source,
                  plugin.manifest,
                  plugin.path,
                  { isSkillMode: false },
                  loadedPaths,
                )

                if (commands.length > 0) {
                  logForDebugging(
                    `Loaded ${commands.length} commands from plugin ${plugin.name} custom path: ${commandPath}`,
                  )
                } else {
                  logForDebugging(
                    `Warning: No commands found in plugin ${plugin.name} custom directory: ${commandPath}. Expected .md files or SKILL.md in subdirectories.`,
                    { level: 'warn' },
                  )
                }
                return commands
              } else if (stats.isFile() && commandPath.endsWith('.md')) {
                if (isDuplicatePath(fs, commandPath, loadedPaths)) {
                  return []
                }

                // Load single command file
                const content = await readCanonicalPluginTextFile(
                  plugin.path,
                  commandPath,
                  'command markdown',
                )
                const { frontmatter, content: markdownContent } =
                  parseFrontmatter(content, commandPath)

                // Check if there's metadata for this command (object-mapping format)
                let commandName: string | undefined
                let metadataOverride: CommandMetadata | undefined

                if (plugin.commandsMetadata) {
                  // Find metadata by matching the command's absolute path to the metadata source
                  // Convert metadata.source (relative to plugin root) to absolute path for comparison
                  for (const [name, metadata] of Object.entries(
                    plugin.commandsMetadata as Record<string, CommandMetadata>,
                  )) {
                    if (metadata.source) {
                      try {
                        const fullMetadataPath =
                          await resolvePluginComponentPath(
                            plugin.path,
                            metadata.source,
                            { component: 'command metadata source' },
                          )
                        if (commandPath === fullMetadataPath) {
                          commandName = `${plugin.name}:${name}`
                          metadataOverride = metadata
                          break
                        }
                      } catch (error) {
                        logForDebugging(
                          `Rejected command metadata source ${metadata.source} for ${plugin.name}: ${error}`,
                          { level: 'warn' },
                        )
                      }
                    }
                  }
                }

                // Fall back to filename-based naming if no metadata
                if (!commandName) {
                  commandName = `${plugin.name}:${basename(commandPath).replace(/\.md$/, '')}`
                }

                // Apply metadata overrides to frontmatter
                const finalFrontmatter = metadataOverride
                  ? {
                      ...frontmatter,
                      ...(metadataOverride.description && {
                        description: metadataOverride.description,
                      }),
                      ...(metadataOverride.argumentHint && {
                        'argument-hint': metadataOverride.argumentHint,
                      }),
                      ...(metadataOverride.model && {
                        model: metadataOverride.model,
                      }),
                      ...(metadataOverride.allowedTools && {
                        'allowed-tools':
                          metadataOverride.allowedTools.join(','),
                      }),
                    }
                  : frontmatter

                const file: PluginMarkdownFile = {
                  filePath: commandPath,
                  baseDir: dirname(commandPath),
                  frontmatter: finalFrontmatter,
                  content: markdownContent,
                }

                const command = createPluginCommand(
                  commandName,
                  file,
                  plugin.source,
                  plugin.manifest,
                  plugin.path,
                  false,
                )

                if (command) {
                  logForDebugging(
                    `Loaded command from plugin ${plugin.name} custom file: ${commandPath}${metadataOverride ? ' (with metadata override)' : ''}`,
                  )
                  return [command]
                }
              }
              return []
            } catch (error) {
              logForDebugging(
                `Failed to load commands from plugin ${plugin.name} custom path ${commandPath}: ${error}`,
                { level: 'error' },
              )
              return []
            }
          }),
        )
        for (const commands of pathResults) {
          pluginCommands.push(...commands)
        }
      }

      // Load commands with inline content (no source file)
      // Note: Commands with source files were already loaded in the previous loop
      // when iterating through commandsPaths. This loop handles metadata entries
      // that specify inline content instead of file references.
      if (plugin.commandsMetadata) {
        for (const [name, metadata] of Object.entries(
          plugin.commandsMetadata as Record<string, CommandMetadata>,
        )) {
          // Only process entries with inline content (no source)
          if (metadata.content && !metadata.source) {
            try {
              // Parse inline content for frontmatter
              const { frontmatter, content: markdownContent } =
                parseFrontmatter(
                  metadata.content,
                  `<inline:${plugin.name}:${name}>`,
                )

              // Apply metadata overrides to frontmatter
              const finalFrontmatter: FrontmatterData = {
                ...frontmatter,
                ...(metadata.description && {
                  description: metadata.description,
                }),
                ...(metadata.argumentHint && {
                  'argument-hint': metadata.argumentHint,
                }),
                ...(metadata.model && {
                  model: metadata.model,
                }),
                ...(metadata.allowedTools && {
                  'allowed-tools': metadata.allowedTools.join(','),
                }),
              }

              const commandName = `${plugin.name}:${name}`
              const file: PluginMarkdownFile = {
                filePath: `<inline:${commandName}>`, // Virtual path for inline content
                baseDir: plugin.path, // Use plugin root as base directory
                frontmatter: finalFrontmatter,
                content: markdownContent,
              }

              const command = createPluginCommand(
                commandName,
                file,
                plugin.source,
                plugin.manifest,
                plugin.path,
                false,
              )

              if (command) {
                pluginCommands.push(command)
                logForDebugging(
                  `Loaded inline content command from plugin ${plugin.name}: ${commandName}`,
                )
              }
            } catch (error) {
              logForDebugging(
                `Failed to load inline content command ${name} from plugin ${plugin.name}: ${error}`,
                { level: 'error' },
              )
            }
          }
        }
      }
      return pluginCommands
    }),
  )

  const allCommands = perPluginCommands.flat()
  logForDebugging(`Total plugin commands loaded: ${allCommands.length}`)
  return allCommands
})

export function clearPluginCommandCache(): void {
  getPluginCommands.cache?.clear?.()
}

/**
 * Loads skills from plugin skills directories
 * Skills are directories containing SKILL.md files
 *
 * Exported for regression testing of the `skillRoot` contract: plugin skills
 * MUST carry `skillRoot` (= the SKILL.md directory) or the catalog projection
 * silently drops them (see `createPluginCommand`). The test
 * mocks `getPluginSkills` and previously *fabricated* skillRoot, hiding the
 * gap — this seam lets a test exercise the real loader.
 */
export async function loadSkillsFromDirectory(
  skillsPath: string,
  pluginName: string,
  sourceName: string,
  pluginManifest: PluginManifest,
  pluginPath: string,
  loadedPaths: Set<string>,
): Promise<Command[]> {
  const fs = getFsImplementation()
  const skills: Command[] = []
  skillsPath = await resolveInternalPluginPath(pluginPath, skillsPath, {
    component: 'skills directory',
  })

  // First, check if skillsPath itself contains SKILL.md (direct skill directory)
  const directSkillCandidate = join(skillsPath, 'SKILL.md')
  let directSkillContent: string | null = null
  let directSkillPath = directSkillCandidate
  try {
    directSkillPath = await resolveInternalPluginPath(
      pluginPath,
      directSkillCandidate,
      { mustExist: false, component: 'skill markdown' },
    )
    directSkillContent = await readCanonicalPluginTextFile(
      pluginPath,
      directSkillPath,
      'skill markdown',
    )
  } catch (e: unknown) {
    if (!isENOENT(e)) {
      logForDebugging(`Failed to load skill from ${directSkillPath}: ${e}`, {
        level: 'error',
      })
      return skills
    }
    // ENOENT: no direct SKILL.md, fall through to scan subdirectories
  }

  if (directSkillContent !== null) {
    // This is a direct skill directory, load the skill from here
    if (isDuplicatePath(fs, directSkillPath, loadedPaths)) {
      return skills
    }
    try {
      const { frontmatter, content: markdownContent } = parseFrontmatter(
        directSkillContent,
        directSkillPath,
      )

      const skillName = `${pluginName}:${basename(skillsPath)}`

      const file: PluginMarkdownFile = {
        filePath: directSkillPath,
        baseDir: dirname(directSkillPath),
        frontmatter,
        content: markdownContent,
      }

      const skill = createPluginCommand(
        skillName,
        file,
        sourceName,
        pluginManifest,
        pluginPath,
        true, // isSkill
        { isSkillMode: true }, // config
      )

      if (skill) {
        skills.push(skill)
      }
    } catch (error) {
      logForDebugging(
        `Failed to load skill from ${directSkillPath}: ${error}`,
        {
          level: 'error',
        },
      )
    }
    return skills
  }

  // Otherwise, scan for subdirectories containing SKILL.md files
  let entries
  try {
    entries = await fs.readdir(skillsPath)
    await revalidatePluginPath(
      pluginPath,
      skillsPath,
      'skills directory',
    )
  } catch (e: unknown) {
    if (!isENOENT(e)) {
      logForDebugging(
        `Failed to load skills from directory ${skillsPath}: ${e}`,
        { level: 'error' },
      )
    }
    return skills
  }

  await Promise.all(
    entries.map(async entry => {
      // Accept both directories and symlinks (symlinks may point to skill directories)
      if (!entry.isDirectory() && !entry.isSymbolicLink()) {
        return
      }

      const skillDirPath = join(skillsPath, entry.name)
      const skillFileCandidate = join(skillDirPath, 'SKILL.md')

      // Try to read SKILL.md directly; skip if it doesn't exist
      let content: string
      let skillFilePath: string
      try {
        skillFilePath = await resolveInternalPluginPath(
          pluginPath,
          skillFileCandidate,
          { mustExist: false, component: 'skill markdown' },
        )
        content = await readCanonicalPluginTextFile(
          pluginPath,
          skillFilePath,
          'skill markdown',
        )
      } catch (e: unknown) {
        if (!isENOENT(e)) {
          logForDebugging(
            `Failed to load skill from ${skillFileCandidate}: ${e}`,
            { level: 'error' },
          )
        }
        return
      }

      if (isDuplicatePath(fs, skillFilePath, loadedPaths)) {
        return
      }

      try {
        const { frontmatter, content: markdownContent } = parseFrontmatter(
          content,
          skillFilePath,
        )

        const skillName = `${pluginName}:${entry.name}`

        const file: PluginMarkdownFile = {
          filePath: skillFilePath,
          baseDir: dirname(skillFilePath),
          frontmatter,
          content: markdownContent,
        }

        const skill = createPluginCommand(
          skillName,
          file,
          sourceName,
          pluginManifest,
          pluginPath,
          true, // isSkill
          { isSkillMode: true }, // config
        )

        if (skill) {
          skills.push(skill)
        }
      } catch (error) {
        logForDebugging(
          `Failed to load skill from ${skillFilePath}: ${error}`,
          { level: 'error' },
        )
      }
    }),
  )

  return skills
}

// Wave D (R2 根修)：可选 targetCwd 决定插件安装态按哪个 cwd 判相关性（透传
// loadAllPluginsCacheOnly → marketplace loader）。memoize 按 targetCwd 键化，
// 零参 = '@process'（进程 cwd 语义 —— src/commands.ts 等既有调用点零改动）；
// worker 的 skills/list 传请求 cwds[0]。clearPluginSkillsCache 整表 clear。
const getPluginSkillsImpl = async (
  targetCwd?: string,
): Promise<Command[]> => {
  // --bare: same gate as getPluginCommands above — honor explicit
  // --plugin-dir, skip marketplace auto-load.
  if (isBareMode() && getInlinePlugins().length === 0) {
    return []
  }
  // Only load skills from enabled plugins
  const { enabled, errors } = await loadAllPluginsCacheOnly(targetCwd)

  if (errors.length > 0) {
    logForDebugging(
      `Plugin loading errors: ${errors.map(e => getPluginErrorMessage(e)).join(', ')}`,
    )
  }

  logForDebugging(
    `getPluginSkills: Processing ${enabled.length} enabled plugins`,
  )

  // Process plugins in parallel; each plugin has its own loadedPaths scope
  const perPluginSkills = await Promise.all(
    enabled.map(async (plugin): Promise<Command[]> => {
      // Track loaded file paths to prevent duplicates within this plugin
      const loadedPaths = new Set<string>()
      const pluginSkills: Command[] = []

      logForDebugging(
        `Checking plugin ${plugin.name}: skillsPath=${plugin.skillsPath ? 'exists' : 'none'}, skillsPaths=${plugin.skillsPaths ? plugin.skillsPaths.length : 0} paths`,
      )
      // Load skills from default skills directory
      if (plugin.skillsPath) {
        logForDebugging(
          `Attempting to load skills from plugin ${plugin.name} default skillsPath: ${plugin.skillsPath}`,
        )
        try {
          const skills = await loadSkillsFromDirectory(
            plugin.skillsPath,
            plugin.name,
            plugin.source,
            plugin.manifest,
            plugin.path,
            loadedPaths,
          )
          pluginSkills.push(...skills)

          logForDebugging(
            `Loaded ${skills.length} skills from plugin ${plugin.name} default directory`,
          )
        } catch (error) {
          logForDebugging(
            `Failed to load skills from plugin ${plugin.name} default directory: ${error}`,
            { level: 'error' },
          )
        }
      }

      // Load skills from additional paths specified in manifest
      if (plugin.skillsPaths) {
        logForDebugging(
          `Attempting to load skills from plugin ${plugin.name} skillsPaths: ${plugin.skillsPaths.join(', ')}`,
        )
        // Process all skillsPaths in parallel. isDuplicatePath is synchronous
        // (check-and-add), so concurrent access to loadedPaths is safe.
        const pathResults = await Promise.all(
          plugin.skillsPaths.map(async (skillPath): Promise<Command[]> => {
            try {
              logForDebugging(
                `Loading from skillPath: ${skillPath} for plugin ${plugin.name}`,
              )
              const skills = await loadSkillsFromDirectory(
                skillPath,
                plugin.name,
                plugin.source,
                plugin.manifest,
                plugin.path,
                loadedPaths,
              )

              logForDebugging(
                `Loaded ${skills.length} skills from plugin ${plugin.name} custom path: ${skillPath}`,
              )
              return skills
            } catch (error) {
              logForDebugging(
                `Failed to load skills from plugin ${plugin.name} custom path ${skillPath}: ${error}`,
                { level: 'error' },
              )
              return []
            }
          }),
        )
        for (const skills of pathResults) {
          pluginSkills.push(...skills)
        }
      }
      return pluginSkills
    }),
  )

  const allSkills = perPluginSkills.flat()
  logForDebugging(`Total plugin skills loaded: ${allSkills.length}`)
  return allSkills
}

export const getPluginSkills = memoize(
  getPluginSkillsImpl,
  (targetCwd?: string) => targetCwd ?? '@process',
)

export function clearPluginSkillsCache(): void {
  getPluginSkills.cache?.clear?.()
}
