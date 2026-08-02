/**
 * Directory copy and versioned cache write operations.
 *
 * Provides recursive directory copy and the logic for writing
 * plugin content into the versioned cache structure.
 */

import {
  readdir,
  realpath,
  stat,
} from 'fs/promises'
import { basename, dirname, join, relative, resolve } from 'path'
import { getFsImplementation } from '../../fsOperations.js'
import {
  copyCanonicalPluginFile,
  revalidatePluginPath,
  resolvePluginComponentPath,
} from '../pluginPathSecurity.js'

/**
 * Recursively copy a directory.
 * Exported for testing purposes.
 */
export async function copyDir(src: string, dest: string): Promise<void> {
  const canonicalSourceRoot = await realpath(src)
  const lexicalDestination = resolve(dest)
  const destinationParent = dirname(lexicalDestination)
  await getFsImplementation().mkdir(destinationParent)
  const safeDestination = await resolvePluginComponentPath(
    destinationParent,
    basename(lexicalDestination),
    {
      mustExist: false,
      rejectSymlinks: true,
      rejectRoot: true,
      component: 'plugin cache destination root',
    },
  )
  await getFsImplementation().mkdir(safeDestination)
  const canonicalDestinationRoot = await realpath(safeDestination)
  await copyDirWithinRoot(
    canonicalSourceRoot,
    canonicalDestinationRoot,
    canonicalSourceRoot,
    canonicalDestinationRoot,
    new Set<string>(),
  )
}

async function copyDirWithinRoot(
  src: string,
  dest: string,
  sourceRoot: string,
  destinationRoot: string,
  activeDirectories: Set<string>,
): Promise<void> {
  if (activeDirectories.has(src)) {
    throw new Error(
      `Plugin cache source contains a directory symlink cycle: ${src}`,
    )
  }
  activeDirectories.add(src)
  try {
    await getFsImplementation().mkdir(dest)

    const entries = await readdir(src, { withFileTypes: true })
    await revalidatePluginPath(sourceRoot, src, 'plugin cache source directory')

    for (const entry of entries) {
      const srcPath = join(src, entry.name)
      const sourceRelativePath = relative(sourceRoot, srcPath)
      const destinationRelativePath = relative(
        destinationRoot,
        join(dest, entry.name),
      )
      const canonicalSourcePath = await resolvePluginComponentPath(
        sourceRoot,
        sourceRelativePath,
        { component: 'plugin cache source' },
      )
      const destPath = await resolvePluginComponentPath(
        destinationRoot,
        destinationRelativePath,
        {
          mustExist: false,
          rejectSymlinks: true,
          component: 'plugin cache destination',
        },
      )

      if (entry.isDirectory()) {
        await getFsImplementation().mkdir(destPath)
        await revalidatePluginPath(
          destinationRoot,
          destPath,
          'plugin cache destination directory',
        )
        await copyDirWithinRoot(
          canonicalSourcePath,
          destPath,
          sourceRoot,
          destinationRoot,
          activeDirectories,
        )
      } else if (entry.isFile()) {
        await copyCanonicalPluginFile(
          sourceRoot,
          canonicalSourcePath,
          destinationRoot,
          destPath,
          'plugin cache file',
        )
      } else if (entry.isSymbolicLink()) {
        // Materialize in-tree links so the cache contains no mutable link edge.
        // resolvePluginComponentPath above already rejected outside/broken links.
        const targetStat = await stat(canonicalSourcePath)
        if (targetStat.isDirectory()) {
          await getFsImplementation().mkdir(destPath)
          await copyDirWithinRoot(
            canonicalSourcePath,
            destPath,
            sourceRoot,
            destinationRoot,
            activeDirectories,
          )
        } else if (targetStat.isFile()) {
          await copyCanonicalPluginFile(
            sourceRoot,
            canonicalSourcePath,
            destinationRoot,
            destPath,
            'plugin cache materialized symlink',
          )
        } else {
          throw new Error(
            `Plugin cache source symlink target is not a file or directory: ${srcPath}`,
          )
        }
      }
    }
  } finally {
    activeDirectories.delete(src)
  }
}
