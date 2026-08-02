/**
 * Plugin installation from various sources.
 *
 * Handles installing plugins from git, npm, GitHub, local paths,
 * and git subdirectories. Also provides the top-level cachePlugin
 * function that dispatches to the appropriate installer.
 */

import { randomUUID } from 'crypto'
import { rename, rm } from 'fs/promises'
import { basename, dirname, join } from 'path'
import { localExecBridge } from 'src/runtime/localProcess.js'
import type { PluginManifest } from '../../../types/plugin.js'
import { logForDebugging } from '../../debug.js'
import { isEnvTruthy } from '../../envUtils.js'
import { errorMessage, isENOENT } from '../../errors.js'
import { pathExists } from '../../file.js'
import { getFsImplementation } from '../../fsOperations.js'
import { jsonParse, jsonStringify } from '../../slowOperations.js'
import { classifyFetchError, logPluginFetch } from '../fetchTelemetry.js'
import { checkGitAvailable } from '../gitAvailability.js'
import { getPluginsDirectory } from '../pluginDirectories.js'
import {
  readCanonicalPluginTextFile,
  resolveInternalPluginPath,
  resolvePluginComponentPath,
} from '../pluginPathSecurity.js'
import { PluginManifestSchema, type PluginSource } from '../schemas.js'
import { copyDir } from './copyAndCache.js'

/**
 * Validate a git URL using Node.js URL parsing
 */
function validateGitUrl(url: string): string {
  try {
    const parsed = new URL(url)
    if (!['https:', 'http:', 'file:'].includes(parsed.protocol)) {
      if (!/^git@[a-zA-Z0-9.-]+:/.test(url)) {
        throw new Error(
          `Invalid git URL protocol: ${parsed.protocol}. Only HTTPS, HTTP, file:// and SSH (git@) URLs are supported.`,
        )
      }
    }
    return url
  } catch {
    if (/^git@[a-zA-Z0-9.-]+:/.test(url)) {
      return url
    }
    throw new Error(`Invalid git URL: ${url}`)
  }
}

/**
 * Install a plugin from npm using a global cache (exported for testing)
 */
export async function installFromNpm(
  packageName: string,
  targetPath: string,
  options: { registry?: string; version?: string } = {},
): Promise<void> {
  const pluginsRoot = getPluginsDirectory()
  await getFsImplementation().mkdir(pluginsRoot)
  const npmCachePath = await resolvePluginComponentPath(
    pluginsRoot,
    'npm-cache',
    {
      mustExist: false,
      rejectSymlinks: true,
      component: 'npm plugin cache root',
    },
  )

  await getFsImplementation().mkdir(npmCachePath)

  const packageSpec = options.version
    ? `${packageName}@${options.version}`
    : packageName
  let packagePath = await resolvePluginComponentPath(
    npmCachePath,
    join('node_modules', packageName),
    {
      mustExist: false,
      rejectSymlinks: true,
      component: 'npm plugin package',
    },
  )
  const needsInstall = !(await pathExists(packagePath))

  if (needsInstall) {
    logForDebugging(`Installing npm package ${packageSpec} to cache`)
    const args = ['install', packageSpec, '--prefix', npmCachePath]
    if (options.registry) {
      args.push('--registry', options.registry)
    }
    const result = await localExecBridge.execCommand({
      command: 'npm',
      args,
      cwd: process.cwd(),
    })

    if (result.code !== 0) {
      throw new Error(`Failed to install npm package: ${result.stderr}`)
    }
  }

  // Re-resolve after npm has materialized node_modules. A package-level
  // symlink created during installation must not turn the earlier missing
  // creation target into an out-of-cache read root.
  packagePath = await resolvePluginComponentPath(
    npmCachePath,
    join('node_modules', packageName),
    { component: 'npm plugin package' },
  )

  await copyDir(packagePath, targetPath)
  logForDebugging(
    `Copied npm package ${packageName} from cache to ${targetPath}`,
  )
}

/**
 * Clone a git repository (exported for testing)
 *
 * @param gitUrl - The git URL to clone
 * @param targetPath - Where to clone the repository
 * @param ref - Optional branch or tag to checkout
 * @param sha - Optional specific commit SHA to checkout
 */
export async function gitClone(
  gitUrl: string,
  targetPath: string,
  ref?: string,
  sha?: string,
): Promise<void> {
  // Use --recurse-submodules to initialize submodules
  // Always start with shallow clone for efficiency
  const args = [
    'clone',
    '--depth',
    '1',
    '--recurse-submodules',
    '--shallow-submodules',
  ]

  // Add --branch flag for specific ref (works for both branches and tags)
  if (ref) {
    args.push('--branch', ref)
  }

  // If sha is specified, use --no-checkout since we'll checkout the SHA separately
  if (sha) {
    args.push('--no-checkout')
  }

  args.push(gitUrl, targetPath)

  const cloneStarted = performance.now()
  const cloneResult = await localExecBridge.execGitCommand({ args })

  if (cloneResult.code !== 0) {
    logPluginFetch(
      'plugin_clone',
      gitUrl,
      'failure',
      performance.now() - cloneStarted,
      classifyFetchError(cloneResult.stderr),
    )
    throw new Error(`Failed to clone repository: ${cloneResult.stderr}`)
  }

  // If sha is specified, fetch and checkout that specific commit
  if (sha) {
    // Try shallow fetch of the specific SHA first (most efficient)
    const shallowFetchResult = await localExecBridge.execGitCommand({
      args: ['fetch', '--depth', '1', 'origin', sha],
      cwd: targetPath,
    })

    if (shallowFetchResult.code !== 0) {
      // Some servers don't support fetching arbitrary SHAs
      // Fall back to unshallow fetch to get full history
      logForDebugging(
        `Shallow fetch of SHA ${sha} failed, falling back to unshallow fetch`,
      )
      const unshallowResult = await localExecBridge.execGitCommand({
        args: ['fetch', '--unshallow'],
        cwd: targetPath,
      })

      if (unshallowResult.code !== 0) {
        logPluginFetch(
          'plugin_clone',
          gitUrl,
          'failure',
          performance.now() - cloneStarted,
          classifyFetchError(unshallowResult.stderr),
        )
        throw new Error(
          `Failed to fetch commit ${sha}: ${unshallowResult.stderr}`,
        )
      }
    }

    // Checkout the specific commit
    const checkoutResult = await localExecBridge.execGitCommand({
      args: ['checkout', sha],
      cwd: targetPath,
    })

    if (checkoutResult.code !== 0) {
      logPluginFetch(
        'plugin_clone',
        gitUrl,
        'failure',
        performance.now() - cloneStarted,
        classifyFetchError(checkoutResult.stderr),
      )
      throw new Error(
        `Failed to checkout commit ${sha}: ${checkoutResult.stderr}`,
      )
    }
  }

  // Fire success only after ALL network ops (clone + optional SHA fetch)
  // complete -- same telemetry-scope discipline as mcpb and marketplace_url.
  logPluginFetch(
    'plugin_clone',
    gitUrl,
    'success',
    performance.now() - cloneStarted,
  )
}

/**
 * Install a plugin from a git URL
 */
async function installFromGit(
  gitUrl: string,
  targetPath: string,
  ref?: string,
  sha?: string,
): Promise<void> {
  const safeUrl = validateGitUrl(gitUrl)
  await gitClone(safeUrl, targetPath, ref, sha)
  const refMessage = ref ? ` (ref: ${ref})` : ''
  logForDebugging(
    `Cloned repository from ${safeUrl}${refMessage} to ${targetPath}`,
  )
}

/**
 * Install a plugin from GitHub
 */
async function installFromGitHub(
  repo: string,
  targetPath: string,
  ref?: string,
  sha?: string,
): Promise<void> {
  if (!/^[a-zA-Z0-9-_.]+\/[a-zA-Z0-9-_.]+$/.test(repo)) {
    throw new Error(
      `Invalid GitHub repository format: ${repo}. Expected format: owner/repo`,
    )
  }
  // Use HTTPS for CCR (no SSH keys), SSH for normal CLI
  const gitUrl = isEnvTruthy(process.env.CRABCODE_REMOTE)
    ? `https://github.com/${repo}.git`
    : `git@github.com:${repo}.git`
  return installFromGit(gitUrl, targetPath, ref, sha)
}

/**
 * Resolve a git-subdir `url` field to a clonable git URL.
 * Accepts GitHub owner/repo shorthand (converted to ssh or https depending on
 * CRABCODE_REMOTE) or any URL that passes validateGitUrl (https, http,
 * file, git@ ssh).
 */
function resolveGitSubdirUrl(url: string): string {
  if (/^[a-zA-Z0-9-_.]+\/[a-zA-Z0-9-_.]+$/.test(url)) {
    return isEnvTruthy(process.env.CRABCODE_REMOTE)
      ? `https://github.com/${url}.git`
      : `git@github.com:${url}.git`
  }
  return validateGitUrl(url)
}

/**
 * Install a plugin from a subdirectory of a git repository (exported for
 * testing).
 *
 * Uses partial clone (--filter=tree:0) + sparse-checkout so only the tree
 * objects along the path and the blobs under it are downloaded. For large
 * monorepos this is dramatically cheaper than a full clone -- the tree objects
 * for a million-file repo can be hundreds of MB, all avoided here.
 *
 * Sequence:
 * 1. clone --depth 1 --filter=tree:0 --no-checkout [--branch ref]
 * 2. sparse-checkout set --cone -- <path>
 * 3. If sha: fetch --depth 1 origin <sha> (fallback: --unshallow), then
 *    checkout <sha>. The partial-clone filter is stored in remote config so
 *    subsequent fetches respect it; --unshallow gets all commits but trees
 *    and blobs remain lazy.
 *    If no sha: checkout HEAD (points to ref if --branch was used).
 * 4. Move <cloneDir>/<path> to targetPath and discard the clone.
 *
 * The clone is ephemeral -- it goes into a sibling temp directory and is
 * removed after the subdir is extracted. targetPath ends up containing only
 * the plugin files with no .git directory.
 */
export async function installFromGitSubdir(
  url: string,
  targetPath: string,
  subdirPath: string,
  ref?: string,
  sha?: string,
): Promise<string | undefined> {
  if (!(await checkGitAvailable())) {
    throw new Error(
      'git-subdir plugin source requires git to be installed and on PATH. ' +
        'Install git (version 2.25 or later for sparse-checkout cone mode) and try again.',
    )
  }

  const gitUrl = resolveGitSubdirUrl(url)
  // Clone into a sibling temp dir (same filesystem -> rename works, no EXDEV).
  const targetParent = dirname(targetPath)
  const safeTargetPath = await resolvePluginComponentPath(
    targetParent,
    basename(targetPath),
    {
      mustExist: false,
      rejectSymlinks: true,
      rejectRoot: true,
      component: 'git plugin cache destination',
    },
  )
  const cloneDir = await resolvePluginComponentPath(
    targetParent,
    `${basename(targetPath)}.clone`,
    {
      mustExist: false,
      rejectSymlinks: true,
      rejectRoot: true,
      component: 'git plugin clone directory',
    },
  )

  const cloneArgs = [
    'clone',
    '--depth',
    '1',
    '--filter=tree:0',
    '--no-checkout',
  ]
  if (ref) {
    cloneArgs.push('--branch', ref)
  }
  cloneArgs.push(gitUrl, cloneDir)

  const cloneResult = await localExecBridge.execGitCommand({ args: cloneArgs })
  if (cloneResult.code !== 0) {
    throw new Error(
      `Failed to clone repository for git-subdir source: ${cloneResult.stderr}`,
    )
  }

  try {
    const sparseResult = await localExecBridge.execGitCommand({
      args: ['sparse-checkout', 'set', '--cone', '--', subdirPath],
      cwd: cloneDir,
    })
    if (sparseResult.code !== 0) {
      throw new Error(
        `git sparse-checkout set failed (git >= 2.25 required for cone mode): ${sparseResult.stderr}`,
      )
    }

    // Capture the resolved commit SHA before discarding the clone. The
    // extracted subdir has no .git, so the caller can't rev-parse it later.
    // If the source specified a full 40-char sha we already know it; otherwise
    // read HEAD (which points to ref's tip after --branch, or the remote
    // default branch if no ref was given).
    let resolvedSha: string | undefined

    if (sha) {
      const fetchSha = await localExecBridge.execGitCommand({
        args: ['fetch', '--depth', '1', 'origin', sha],
        cwd: cloneDir,
      })
      if (fetchSha.code !== 0) {
        logForDebugging(
          `Shallow fetch of SHA ${sha} failed for git-subdir, falling back to unshallow fetch`,
        )
        const unshallow = await localExecBridge.execGitCommand({
          args: ['fetch', '--unshallow'],
          cwd: cloneDir,
        })
        if (unshallow.code !== 0) {
          throw new Error(`Failed to fetch commit ${sha}: ${unshallow.stderr}`)
        }
      }
      const checkout = await localExecBridge.execGitCommand({
        args: ['checkout', sha],
        cwd: cloneDir,
      })
      if (checkout.code !== 0) {
        throw new Error(`Failed to checkout commit ${sha}: ${checkout.stderr}`)
      }
      resolvedSha = sha
    } else {
      // checkout HEAD materializes the working tree (this is where blobs are
      // lazy-fetched -- the slow, network-bound step). It doesn't move HEAD;
      // --branch at clone time already positioned it. rev-parse HEAD is a
      // purely read-only ref lookup (no index lock), so it runs safely in
      // parallel with checkout and we avoid waiting on the network for it.
      const [checkout, revParse] = await localExecBridge.execGitCommandBatch([
        { args: ['checkout', 'HEAD'], cwd: cloneDir },
        { args: ['rev-parse', 'HEAD'], cwd: cloneDir },
      ])
      if (checkout.code !== 0) {
        throw new Error(
          `git checkout after sparse-checkout failed: ${checkout.stderr}`,
        )
      }
      if (revParse.code === 0) {
        resolvedSha = revParse.stdout.trim()
      }
    }

    // Path traversal guard: resolve+verify the subdir stays inside cloneDir
    // before moving it out. rename ENOENT is wrapped with a friendlier
    // message that references the source path, not internal temp dirs.
    const resolvedSubdir = await resolvePluginComponentPath(
      cloneDir,
      subdirPath,
      { component: 'git plugin subdirectory' },
    )
    try {
      const activeTargetPath = await resolvePluginComponentPath(
        targetParent,
        basename(safeTargetPath),
        {
          mustExist: false,
          rejectSymlinks: true,
          rejectRoot: true,
          component: 'git plugin cache destination',
        },
      )
      await rename(resolvedSubdir, activeTargetPath)
    } catch (e: unknown) {
      if (isENOENT(e)) {
        throw new Error(
          `Subdirectory '${subdirPath}' not found in repository ${gitUrl}${ref ? ` (ref: ${ref})` : ''}. ` +
            'Check that the path is correct and exists at the specified ref/sha.',
        )
      }
      throw e
    }

    const refMsg = ref ? ` ref=${ref}` : ''
    const shaMsg = resolvedSha ? ` sha=${resolvedSha}` : ''
    logForDebugging(
      `Extracted subdir ${subdirPath} from ${gitUrl}${refMsg}${shaMsg} to ${targetPath}`,
    )
    return resolvedSha
  } finally {
    try {
      const safeCloneDir = await resolvePluginComponentPath(
        targetParent,
        basename(cloneDir),
        {
          mustExist: false,
          rejectSymlinks: true,
          rejectRoot: true,
          component: 'git plugin clone cleanup',
        },
      )
      await rm(safeCloneDir, { recursive: true, force: true })
    } catch (cleanupError) {
      logForDebugging(
        `Refused or failed to clean git plugin clone directory: ${cleanupError}`,
        { level: 'warn' },
      )
    }
  }
}

/**
 * Install a plugin from a local path
 */
async function installFromLocal(
  sourcePath: string,
  targetPath: string,
): Promise<void> {
  if (!(await pathExists(sourcePath))) {
    throw new Error(`Source path does not exist: ${sourcePath}`)
  }

  await copyDir(sourcePath, targetPath)

  const gitPath = await resolvePluginComponentPath(targetPath, '.git', {
    mustExist: false,
    rejectSymlinks: true,
    rejectRoot: true,
    component: 'plugin cache git metadata',
  })
  await rm(gitPath, { recursive: true, force: true })
}

/**
 * Generate a temporary cache name for a plugin
 */
export function generateTemporaryCacheNameForPlugin(
  source: PluginSource,
): string {
  const timestamp = Date.now()
  const random = Math.random().toString(36).substring(2, 8)

  let prefix: string

  if (typeof source === 'string') {
    prefix = 'local'
  } else {
    switch (source.source) {
      case 'npm':
        prefix = 'npm'
        break
      case 'pip':
        prefix = 'pip'
        break
      case 'github':
        prefix = 'github'
        break
      case 'url':
        prefix = 'git'
        break
      case 'git-subdir':
        prefix = 'subdir'
        break
      default:
        prefix = 'unknown'
    }
  }

  return `temp_${prefix}_${timestamp}_${random}`
}

/**
 * Cache a plugin from an external source
 */
export async function cachePlugin(
  source: PluginSource,
  options?: {
    manifest?: PluginManifest
    /**
     * Keep the unique temp generation unpublished and return ownership to the
     * caller. Used by the coordinated install transaction so concurrent
     * prepares with the same manifest name cannot delete or overwrite one
     * another. Legacy callers retain the stable-name publication behavior.
     */
    preserveTemporaryPath?: boolean
  },
): Promise<{ path: string; manifest: PluginManifest; gitCommitSha?: string }> {
  const pluginsRoot = getPluginsDirectory()
  await getFsImplementation().mkdir(pluginsRoot)
  const cachePath = await resolvePluginComponentPath(pluginsRoot, 'cache', {
    mustExist: false,
    rejectSymlinks: true,
    component: 'plugin cache root',
  })
  await getFsImplementation().mkdir(cachePath)

  const tempName = options?.preserveTemporaryPath
    ? `.plugin-install-${process.pid}-${randomUUID()}`
    : generateTemporaryCacheNameForPlugin(source)
  let tempPath = await resolvePluginComponentPath(cachePath, tempName, {
    mustExist: false,
    rejectSymlinks: true,
    rejectRoot: true,
    component: 'temporary plugin cache',
  })

  let shouldCleanup = false
  let gitCommitSha: string | undefined

  try {
    logForDebugging(
      `Caching plugin from source: ${jsonStringify(source)} to temporary path ${tempPath}`,
    )

    shouldCleanup = true

    if (typeof source === 'string') {
      await installFromLocal(source, tempPath)
    } else {
      switch (source.source) {
        case 'npm':
          await installFromNpm(source.package, tempPath, {
            registry: source.registry,
            version: source.version,
          })
          break
        case 'github':
          await installFromGitHub(source.repo, tempPath, source.ref, source.sha)
          break
        case 'url':
          await installFromGit(source.url, tempPath, source.ref, source.sha)
          break
        case 'git-subdir':
          gitCommitSha = await installFromGitSubdir(
            source.url,
            tempPath,
            source.path,
            source.ref,
            source.sha,
          )
          break
        case 'pip':
          throw new Error('Python package plugins are not yet supported')
        default:
          throw new Error(`Unsupported plugin source type`)
      }
    }
  } catch (error) {
    if (shouldCleanup && (await pathExists(tempPath))) {
      logForDebugging(`Cleaning up failed installation at ${tempPath}`)
      try {
        const safeTempPath = await resolveInternalPluginPath(
          cachePath,
          tempPath,
          {
            rejectSymlinks: true,
            rejectRoot: true,
            component: 'temporary plugin cache cleanup',
          },
        )
        await rm(safeTempPath, { recursive: true, force: true })
      } catch (cleanupError) {
        logForDebugging(`Failed to clean up installation: ${cleanupError}`, {
          level: 'error',
        })
      }
    }
    throw error
  }

  try {
    tempPath = await resolveInternalPluginPath(cachePath, tempPath, {
      rejectSymlinks: true,
      rejectRoot: true,
      component: 'materialized temporary plugin cache',
    })

    const manifestPath = await resolvePluginComponentPath(
      tempPath,
      '.crabcode-plugin/plugin.json',
      { mustExist: false, component: 'plugin manifest' },
    )
    const legacyManifestPath = await resolvePluginComponentPath(
      tempPath,
      'plugin.json',
      { mustExist: false, component: 'legacy plugin manifest' },
    )
    let manifest: PluginManifest

    if (await pathExists(manifestPath)) {
      try {
        const content = await readCanonicalPluginTextFile(
          tempPath,
          manifestPath,
          'plugin manifest',
        )
        const parsed = jsonParse(content)
        const result = PluginManifestSchema().safeParse(parsed)

        if (result.success) {
          manifest = result.data
        } else {
          // Manifest exists but is invalid - throw error
          const errors = result.error.issues
            .map((err) => `${err.path.join('.')}: ${err.message}`)
            .join(', ')

          logForDebugging(`Invalid manifest at ${manifestPath}: ${errors}`, {
            level: 'error',
          })

          throw new Error(
            `Plugin has an invalid manifest file at ${manifestPath}. Validation errors: ${errors}`,
          )
        }
      } catch (error) {
        // Check if this is a validation error we just threw
        if (
          error instanceof Error &&
          error.message.includes('invalid manifest file')
        ) {
          throw error
        }

        // JSON parse error
        const errorMsg = errorMessage(error)
        logForDebugging(
          `Failed to parse manifest at ${manifestPath}: ${errorMsg}`,
          {
            level: 'error',
          },
        )

        throw new Error(
          `Plugin has a corrupt manifest file at ${manifestPath}. JSON parse error: ${errorMsg}`,
        )
      }
    } else if (await pathExists(legacyManifestPath)) {
      try {
        const content = await readCanonicalPluginTextFile(
          tempPath,
          legacyManifestPath,
          'legacy plugin manifest',
        )
        const parsed = jsonParse(content)
        const result = PluginManifestSchema().safeParse(parsed)

        if (result.success) {
          manifest = result.data
        } else {
          // Manifest exists but is invalid - throw error
          const errors = result.error.issues
            .map((err) => `${err.path.join('.')}: ${err.message}`)
            .join(', ')

          logForDebugging(
            `Invalid legacy manifest at ${legacyManifestPath}: ${errors}`,
            { level: 'error' },
          )

          throw new Error(
            `Plugin has an invalid manifest file at ${legacyManifestPath}. Validation errors: ${errors}`,
          )
        }
      } catch (error) {
        // Check if this is a validation error we just threw
        if (
          error instanceof Error &&
          error.message.includes('invalid manifest file')
        ) {
          throw error
        }

        // JSON parse error
        const errorMsg = errorMessage(error)
        logForDebugging(
          `Failed to parse legacy manifest at ${legacyManifestPath}: ${errorMsg}`,
          {
            level: 'error',
          },
        )

        throw new Error(
          `Plugin has a corrupt manifest file at ${legacyManifestPath}. JSON parse error: ${errorMsg}`,
        )
      }
    } else {
      manifest = options?.manifest || {
        name: tempName,
        description: `Plugin cached from ${typeof source === 'string' ? source : source.source}`,
      }
    }

    if (options?.preserveTemporaryPath) {
      logForDebugging(
        `Prepared plugin ${manifest.name} in unpublished cache generation ${tempPath}`,
      )
      return {
        path: tempPath,
        manifest,
        ...(gitCommitSha && { gitCommitSha }),
      }
    }

    const finalName = manifest.name.replace(/[^a-zA-Z0-9-_]/g, '-')
    if (!finalName) {
      throw new Error('Plugin manifest name does not produce a cache-safe name')
    }
    const finalPath = await resolveInternalPluginPath(
      cachePath,
      join(cachePath, finalName),
      {
        mustExist: false,
        rejectSymlinks: true,
        rejectRoot: true,
        component: 'plugin cache destination',
      },
    )

    if (await pathExists(finalPath)) {
      logForDebugging(`Removing old cached version at ${finalPath}`)
      await rm(finalPath, { recursive: true, force: true })
    }

    const activeTempPath = await resolveInternalPluginPath(
      cachePath,
      tempPath,
      {
        rejectSymlinks: true,
        rejectRoot: true,
        component: 'temporary plugin cache',
      },
    )
    await rename(activeTempPath, finalPath)

    logForDebugging(
      `Successfully cached plugin ${manifest.name} to ${finalPath}`,
    )

    return {
      path: finalPath,
      manifest,
      ...(gitCommitSha && { gitCommitSha }),
    }
  } catch (error) {
    if (options?.preserveTemporaryPath) {
      try {
        const ownedTempPath = await resolveInternalPluginPath(
          cachePath,
          tempPath,
          {
            mustExist: false,
            rejectSymlinks: true,
            rejectRoot: true,
            component: 'unpublished plugin preparation cleanup',
          },
        )
        await rm(ownedTempPath, { recursive: true, force: true })
      } catch (cleanupError) {
        logForDebugging(
          `Failed to clean unpublished plugin preparation: ${cleanupError}`,
          { level: 'warn' },
        )
      }
    }
    throw error
  }
}
