import { constants } from 'fs'
import { access, realpath, stat } from 'fs/promises'
import { delimiter, extname, isAbsolute, join } from 'path'
import type {
  McpServerConfig,
  McpStdioServerConfig,
} from './types.js'
import { resolvePluginComponentPath } from '../../utils/plugins/pluginPathSecurity.js'

export type PluginStdioPreflightResult =
  | {
      state: 'ready'
      config: McpStdioServerConfig
      cwd: string
      shell: false
    }
  | {
      state: 'requiresDependency'
      command: string
      reason: string
    }

export interface PluginStdioPreflightOptions {
  env?: Readonly<Record<string, string | undefined>>
  platform?: NodeJS.Platform
}

async function canonicalExecutable(
  candidate: string,
  platform: NodeJS.Platform,
): Promise<string | null> {
  try {
    const canonical = await realpath(candidate)
    if (!(await stat(canonical)).isFile()) return null
    // Windows does not expose POSIX executable bits. Existence + PATHEXT is
    // the platform's executable contract; POSIX uses X_OK.
    await access(
      canonical,
      platform === 'win32' ? constants.F_OK : constants.X_OK,
    )
    return canonical
  } catch {
    return null
  }
}

export function executableNamesForPlatform(
  command: string,
  platform: NodeJS.Platform,
  env: Readonly<Record<string, string | undefined>>,
): string[] {
  if (platform !== 'win32' || extname(command).length > 0) return [command]
  const pathExt = env.PATHEXT ?? '.COM;.EXE;.BAT;.CMD'
  return pathExt
    .split(';')
    .map(ext => ext.trim())
    .filter(Boolean)
    .map(ext => `${command}${ext.toLowerCase()}`)
}

async function resolveBareExecutable(
  command: string,
  env: Readonly<Record<string, string | undefined>>,
  platform: NodeJS.Platform,
): Promise<string | null> {
  const pathValue = env.PATH ?? env.Path ?? env.path ?? ''
  const names = executableNamesForPlatform(command, platform, env)
  const pathDelimiter = platform === 'win32' ? ';' : delimiter
  for (const directory of pathValue.split(pathDelimiter).filter(Boolean)) {
    for (const name of names) {
      const found = await canonicalExecutable(join(directory, name), platform)
      if (found) return found
    }
  }
  return null
}

function hasPathSeparator(command: string): boolean {
  return command.includes('/') || command.includes('\\')
}

/**
 * Resolve a plugin stdio command without spawning it or executing `--version`.
 * The returned command is an absolute canonical executable and `cwd` is the
 * canonical S2-contained plugin root. No shell/prefix expansion is permitted.
 */
export async function preflightPluginStdio(
  config: McpServerConfig,
  pluginRoot: string,
  options: PluginStdioPreflightOptions = {},
): Promise<PluginStdioPreflightResult> {
  if (config.type !== undefined && config.type !== 'stdio') {
    throw new Error('plugin stdio preflight requires a stdio config')
  }
  const stdio = config as McpStdioServerConfig
  const env = options.env ?? process.env
  const platform = options.platform ?? process.platform
  const canonicalRoot = await resolvePluginComponentPath(pluginRoot, '.', {
    component: 'plugin MCP cwd',
  })

  let command: string | null = null

  if (stdio.command === 'bun' && env.CRABCODE_BUN_BIN) {
    command = await canonicalExecutable(env.CRABCODE_BUN_BIN, platform)
  }

  if (!command && isAbsolute(stdio.command)) {
    command = await canonicalExecutable(stdio.command, platform)
  } else if (!command && hasPathSeparator(stdio.command)) {
    try {
      const contained = await resolvePluginComponentPath(
        canonicalRoot,
        stdio.command,
        { component: 'plugin MCP executable' },
      )
      command = await canonicalExecutable(contained, platform)
    } catch {
      command = null
    }
  } else if (!command) {
    command = await resolveBareExecutable(stdio.command, env, platform)
  }

  if (!command) {
    return {
      state: 'requiresDependency',
      command: stdio.command,
      reason: `Required runtime '${stdio.command}' is not available`,
    }
  }

  return {
    state: 'ready',
    config: { ...stdio, command },
    cwd: canonicalRoot,
    shell: false,
  }
}
