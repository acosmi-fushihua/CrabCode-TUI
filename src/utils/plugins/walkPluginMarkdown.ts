import { join } from 'path'
import { logForDebugging } from '../debug.js'
import { getFsImplementation } from '../fsOperations.js'
import {
  revalidatePluginPath,
  resolveInternalPluginPath,
} from './pluginPathSecurity.js'

const SKILL_MD_RE = /^skill\.md$/i

/**
 * Recursively walk a plugin directory, invoking onFile for each .md file.
 *
 * The namespace array tracks the subdirectory path relative to the root
 * (e.g., ['foo', 'bar'] for root/foo/bar/file.md). Callers that don't need
 * namespacing can ignore the second argument.
 *
 * When stopAtSkillDir is true and a directory contains SKILL.md, onFile is
 * called for all .md files in that directory but subdirectories are not
 * scanned — skill directories are leaf containers.
 *
 * Readdir errors are swallowed with a debug log so one bad directory doesn't
 * abort a plugin load.
 */
export async function walkPluginMarkdown(
  rootDir: string,
  onFile: (fullPath: string, namespace: string[]) => Promise<void>,
  opts: {
    stopAtSkillDir?: boolean
    logLabel?: string
    pluginRoot?: string
  } = {},
): Promise<void> {
  const fs = getFsImplementation()
  const label = opts.logLabel ?? 'plugin'

  async function scan(dirPath: string, namespace: string[]): Promise<void> {
    try {
      const safeDirPath = opts.pluginRoot
        ? await resolveInternalPluginPath(opts.pluginRoot, dirPath, {
            component: `${label} scan directory`,
          })
        : dirPath
      const entries = await fs.readdir(safeDirPath)
      if (opts.pluginRoot) {
        await revalidatePluginPath(
          opts.pluginRoot,
          safeDirPath,
          `${label} scan directory`,
        )
      }

      if (
        opts.stopAtSkillDir &&
        entries.some(e => e.isFile() && SKILL_MD_RE.test(e.name))
      ) {
        // Skill directory: collect .md files here, don't recurse.
        await Promise.all(
          entries.map(async entry => {
            if (!entry.isFile() || !entry.name.toLowerCase().endsWith('.md')) {
              return
            }
            const fullPath = join(safeDirPath, entry.name)
            const safeFullPath = opts.pluginRoot
              ? await resolveInternalPluginPath(opts.pluginRoot, fullPath, {
                  component: `${label} markdown`,
                })
              : fullPath
            await onFile(safeFullPath, namespace)
          }),
        )
        return
      }

      await Promise.all(
        entries.map(async entry => {
          const fullPath = join(safeDirPath, entry.name)
          if (entry.isDirectory()) {
            await scan(fullPath, [...namespace, entry.name])
            return
          }
          if (entry.isFile() && entry.name.toLowerCase().endsWith('.md')) {
            const safeFullPath = opts.pluginRoot
              ? await resolveInternalPluginPath(opts.pluginRoot, fullPath, {
                  component: `${label} markdown`,
                })
              : fullPath
            await onFile(safeFullPath, namespace)
          }
        }),
      )
    } catch (error) {
      logForDebugging(
        `Failed to scan ${label} directory ${dirPath}: ${error}`,
        { level: 'error' },
      )
    }
  }

  await scan(rootDir, [])
}
