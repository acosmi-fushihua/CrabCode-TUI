import { constants } from 'fs'
import { lstat, open, realpath, stat } from 'fs/promises'
import {
  isAbsolute,
  posix,
  relative,
  resolve,
  sep,
  win32,
} from 'path'
import { getErrnoCode } from '../errors.js'

export type PluginPathSecurityReason =
  | 'absolute-path'
  | 'path-escape'
  | 'path-missing'
  | 'path-raced'
  | 'path-symlink'
  | 'path-unresolvable'

export class PluginPathSecurityError extends Error {
  readonly name = 'PluginPathSecurityError'

  constructor(
    readonly reason: PluginPathSecurityReason,
    readonly pluginRoot: string,
    readonly requestedPath: string,
    detail: string,
  ) {
    super(`Plugin path rejected (${reason}): ${detail}`)
  }
}

export type ResolvePluginPathOptions = {
  /** Existing runtime inputs default to true. Creation targets opt into false. */
  mustExist?: boolean
  /** Included in diagnostics only; it does not alter policy. */
  component?: string
  /** Mutation targets reject every existing symlink/junction below root. */
  rejectSymlinks?: boolean
  /** Destructive targets must never collapse to the trusted root itself. */
  rejectRoot?: boolean
}

type CanonicalPluginWriteOptions = {
  exclusive?: boolean
  /** Applied through the opened descriptor so chmod cannot follow a new link. */
  mode?: number
  /** Preserve the historical fail-soft behavior for archive exec-bit restore. */
  bestEffortMode?: boolean
}

type CanonicalPathState = {
  canonicalPath: string
  exists: boolean
}

function isAbsoluteOnAnySupportedPlatform(value: string): boolean {
  return (
    isAbsolute(value) ||
    posix.isAbsolute(value) ||
    win32.isAbsolute(value) ||
    /^[A-Za-z]:/.test(value)
  )
}

/**
 * A plugin may be installed on a different OS from the one on which it was
 * authored. Treat both slash styles as separators when checking whether a
 * relative manifest path climbs above its root, so `..\\secret` cannot be
 * accepted on POSIX and become an escape when the same plugin is loaded on
 * Windows.
 */
function escapesPortableRelativeRoot(value: string): boolean {
  const normalized = posix.normalize(value.replaceAll('\\', '/'))
  return normalized === '..' || normalized.startsWith('../')
}

function isContained(root: string, candidate: string): boolean {
  const rel = relative(root, candidate)
  return (
    rel === '' ||
    (rel !== '..' &&
      !rel.startsWith(`..${sep}`) &&
      !isAbsoluteOnAnySupportedPlatform(rel))
  )
}

function pathError(
  reason: PluginPathSecurityReason,
  pluginRoot: string,
  requestedPath: string,
  component: string,
  detail: string,
): PluginPathSecurityError {
  return new PluginPathSecurityError(
    reason,
    pluginRoot,
    requestedPath,
    `${component} ${detail}`,
  )
}

/**
 * Inspect an existing path, or resolve its deepest existing ancestor and
 * append the still-missing tail.
 *
 * A dangling symlink is not a "missing" component: lstat sees it, realpath
 * cannot prove its destination, so it is rejected instead of being treated as
 * a safe directory that may later be retargeted.
 */
async function inspectCanonicalPath(
  candidate: string,
  pluginRoot: string,
  requestedPath: string,
  component: string,
): Promise<CanonicalPathState> {
  try {
    return {
      canonicalPath: await realpath(candidate),
      exists: true,
    }
  } catch (error) {
    const code = getErrnoCode(error)
    if (code !== 'ENOENT' && code !== 'ENOTDIR') {
      throw pathError(
        'path-unresolvable',
        pluginRoot,
        requestedPath,
        component,
        `target cannot be canonicalized: ${candidate}`,
      )
    }
  }

  let ancestor = candidate
  for (;;) {
    let lexicalEntryExists = false
    try {
      await lstat(ancestor)
      lexicalEntryExists = true
    } catch (error) {
      const code = getErrnoCode(error)
      if (code !== 'ENOENT' && code !== 'ENOTDIR') {
        throw pathError(
          'path-unresolvable',
          pluginRoot,
          requestedPath,
          component,
          `ancestor cannot be inspected: ${ancestor}`,
        )
      }
    }

    if (lexicalEntryExists) {
      let canonicalAncestor: string
      try {
        canonicalAncestor = await realpath(ancestor)
      } catch {
        // lstat succeeded, so this is a dangling/cyclic symlink or an
        // inaccessible existing entry, not an ordinary missing component.
        throw pathError(
          'path-unresolvable',
          pluginRoot,
          requestedPath,
          component,
          `existing ancestor cannot be canonicalized: ${ancestor}`,
        )
      }

      const tail = relative(ancestor, candidate)
      let canonicalAncestorStat: Awaited<ReturnType<typeof stat>>
      try {
        canonicalAncestorStat = await stat(canonicalAncestor)
      } catch {
        throw pathError(
          'path-unresolvable',
          pluginRoot,
          requestedPath,
          component,
          `canonical ancestor cannot be inspected: ${canonicalAncestor}`,
        )
      }
      if (tail !== '' && !canonicalAncestorStat.isDirectory()) {
        throw pathError(
          'path-unresolvable',
          pluginRoot,
          requestedPath,
          component,
          `existing ancestor is not a directory: ${ancestor}`,
        )
      }
      return {
        canonicalPath:
          tail === ''
            ? canonicalAncestor
            : resolve(canonicalAncestor, tail),
        exists: tail === '',
      }
    }

    const parent = resolve(ancestor, '..')
    if (parent === ancestor) {
      throw pathError(
        'path-unresolvable',
        pluginRoot,
        requestedPath,
        component,
        `no existing ancestor could be canonicalized for: ${candidate}`,
      )
    }
    ancestor = parent
  }
}

async function canonicalizeRoot(
  pluginRoot: string,
  requestedPath: string,
  component: string,
): Promise<string> {
  const rootState = await inspectCanonicalPath(
    pluginRoot,
    pluginRoot,
    requestedPath,
    component,
  )
  if (!rootState.exists) {
    throw pathError(
      'path-missing',
      pluginRoot,
      requestedPath,
      component,
      `root cannot be canonicalized: ${pluginRoot}`,
    )
  }

  try {
    if (!(await stat(rootState.canonicalPath)).isDirectory()) {
      throw pathError(
        'path-unresolvable',
        pluginRoot,
        requestedPath,
        component,
        `root is not a directory: ${pluginRoot}`,
      )
    }
  } catch (error) {
    if (error instanceof PluginPathSecurityError) throw error
    throw pathError(
      'path-unresolvable',
      pluginRoot,
      requestedPath,
      component,
      `root cannot be inspected: ${pluginRoot}`,
    )
  }

  return rootState.canonicalPath
}

async function assertNoSymlinkSegments(
  lexicalRoot: string,
  lexicalCandidate: string,
  pluginRoot: string,
  requestedPath: string,
  component: string,
): Promise<void> {
  const rel = relative(lexicalRoot, lexicalCandidate)
  if (rel === '') return
  let current = lexicalRoot
  for (const segment of rel.split(sep)) {
    current = resolve(current, segment)
    try {
      const entry = await lstat(current)
      if (entry.isSymbolicLink()) {
        throw pathError(
          'path-symlink',
          pluginRoot,
          requestedPath,
          component,
          `mutation target crosses a symlink or junction: ${current}`,
        )
      }
    } catch (error) {
      if (error instanceof PluginPathSecurityError) throw error
      const code = getErrnoCode(error)
      if (code === 'ENOENT' || code === 'ENOTDIR') return
      throw pathError(
        'path-unresolvable',
        pluginRoot,
        requestedPath,
        component,
        `mutation target segment cannot be inspected: ${current}`,
      )
    }
  }
}

/**
 * Resolve a manifest/default component path below a plugin root.
 *
 * The caller MUST use the returned canonical path for the subsequent read or
 * scan. Rejoining `pluginRoot` with `relativePath` after validation would
 * reintroduce a symlink-retarget race. Node does not expose openat(2) with a
 * directory fd, so replacement of an already-canonical ancestor by a process
 * that can mutate the plugin/cache tree remains an OS-level TOCTOU boundary;
 * application-controlled plugin/cache roots are therefore still required.
 */
export async function resolvePluginComponentPath(
  pluginRoot: string,
  relativePath: string,
  options: ResolvePluginPathOptions = {},
): Promise<string> {
  const component = options.component ?? 'component path'
  if (
    relativePath.length === 0 ||
    relativePath.includes('\0') ||
    isAbsoluteOnAnySupportedPlatform(relativePath)
  ) {
    throw pathError(
      'absolute-path',
      pluginRoot,
      relativePath,
      component,
      `must be a non-empty relative path: ${relativePath}`,
    )
  }

  if (escapesPortableRelativeRoot(relativePath)) {
    throw pathError(
      'path-escape',
      pluginRoot,
      relativePath,
      component,
      `escapes the plugin root using a portable path separator: ${relativePath}`,
    )
  }

  const lexicalRoot = resolve(pluginRoot)
  const lexicalCandidate = resolve(lexicalRoot, relativePath)
  if (!isContained(lexicalRoot, lexicalCandidate)) {
    throw pathError(
      'path-escape',
      pluginRoot,
      relativePath,
      component,
      `escapes the plugin root before symlink resolution: ${lexicalCandidate}`,
    )
  }

  const canonicalRoot = await canonicalizeRoot(
    pluginRoot,
    relativePath,
    component,
  )
  if (options.rejectSymlinks) {
    await assertNoSymlinkSegments(
      lexicalRoot,
      lexicalCandidate,
      pluginRoot,
      relativePath,
      component,
    )
  }
  const candidateState = await inspectCanonicalPath(
    lexicalCandidate,
    pluginRoot,
    relativePath,
    component,
  )

  if (!isContained(canonicalRoot, candidateState.canonicalPath)) {
    throw pathError(
      'path-escape',
      pluginRoot,
      relativePath,
      component,
      `resolves outside the plugin root: ${candidateState.canonicalPath}`,
    )
  }

  if (options.rejectRoot && candidateState.canonicalPath === canonicalRoot) {
    throw pathError(
      'path-escape',
      pluginRoot,
      relativePath,
      component,
      `must resolve below, not to, the trusted root: ${canonicalRoot}`,
    )
  }

  if ((options.mustExist ?? true) && !candidateState.exists) {
    throw pathError(
      'path-missing',
      pluginRoot,
      relativePath,
      component,
      `target cannot be canonicalized: ${lexicalCandidate}`,
    )
  }

  return candidateState.canonicalPath
}

/**
 * Validate an internally generated absolute path (for example an MCPB cache
 * metadata `extractedPath`) against a plugin/cache root. Manifest input must
 * use resolvePluginComponentPath instead, which rejects absolute paths.
 */
export async function resolveInternalPluginPath(
  root: string,
  candidatePath: string,
  options: ResolvePluginPathOptions = {},
): Promise<string> {
  const component = options.component ?? 'internal plugin path'
  const canonicalRoot = await canonicalizeRoot(root, candidatePath, component)
  const lexicalRoot = resolve(root)
  const absoluteCandidate = isAbsoluteOnAnySupportedPlatform(candidatePath)
    ? resolve(candidatePath)
    : resolve(root, candidatePath)
  if (options.rejectSymlinks) {
    const scanRoot = isContained(canonicalRoot, absoluteCandidate)
      ? canonicalRoot
      : lexicalRoot
    if (isContained(scanRoot, absoluteCandidate)) {
      await assertNoSymlinkSegments(
        scanRoot,
        absoluteCandidate,
        root,
        candidatePath,
        component,
      )
    }
  }
  const candidateState = await inspectCanonicalPath(
    absoluteCandidate,
    root,
    candidatePath,
    component,
  )
  if (!isContained(canonicalRoot, candidateState.canonicalPath)) {
    throw pathError(
      'path-escape',
      root,
      candidatePath,
      component,
      `resolves outside the trusted root: ${candidateState.canonicalPath}`,
    )
  }
  if (options.rejectRoot && candidateState.canonicalPath === canonicalRoot) {
    throw pathError(
      'path-escape',
      root,
      candidatePath,
      component,
      `must resolve below, not to, the trusted root: ${canonicalRoot}`,
    )
  }
  if ((options.mustExist ?? true) && !candidateState.exists) {
    throw pathError(
      'path-missing',
      root,
      candidatePath,
      component,
      `target cannot be canonicalized: ${absoluteCandidate}`,
    )
  }
  return candidateState.canonicalPath
}

/**
 * Re-check a canonical path immediately after an asynchronous read/scan.
 * Callers must discard the bytes/entries they just obtained if this throws.
 * This closes the application-level validate-one-path/read-another race (for
 * example, a symlinked parent retargeted between realpath and readFile).
 */
export async function revalidatePluginPath(
  root: string,
  canonicalPath: string,
  component = 'plugin path',
): Promise<void> {
  const revalidated = await resolveInternalPluginPath(root, canonicalPath, {
    component,
  })
  const stable =
    process.platform === 'win32'
      ? revalidated.toLowerCase() === canonicalPath.toLowerCase()
      : revalidated === canonicalPath
  if (!stable) {
    throw pathError(
      'path-raced',
      root,
      canonicalPath,
      component,
      `changed after validation: ${canonicalPath} -> ${revalidated}`,
    )
  }
}

async function openStablePluginFile(
  root: string,
  canonicalPath: string,
  component: string,
) {
  const expected = await stat(canonicalPath, { bigint: true })
  if (!expected.isFile()) {
    throw pathError(
      'path-unresolvable',
      root,
      canonicalPath,
      component,
      `is not a regular file: ${canonicalPath}`,
    )
  }

  // O_NOFOLLOW pins the final component on POSIX. Windows does not expose an
  // equivalent FILE_FLAG_OPEN_REPARSE_POINT through fs.open; the identity and
  // post-open realpath checks below still reject junction/parent retargets.
  const noFollow = process.platform === 'win32' ? 0 : constants.O_NOFOLLOW
  const handle = await open(canonicalPath, constants.O_RDONLY | noFollow)
  try {
    const opened = await handle.stat({ bigint: true })
    if (opened.dev !== expected.dev || opened.ino !== expected.ino) {
      throw pathError(
        'path-raced',
        root,
        canonicalPath,
        component,
        `file identity changed while it was opened: ${canonicalPath}`,
      )
    }
    await revalidatePluginPath(root, canonicalPath, component)
    return handle
  } catch (error) {
    await handle.close()
    throw error
  }
}

function samePath(left: string, right: string): boolean {
  return process.platform === 'win32'
    ? left.toLowerCase() === right.toLowerCase()
    : left === right
}

async function assertOpenPathIdentity(
  root: string,
  canonicalPath: string,
  component: string,
  opened: Awaited<ReturnType<typeof open>>,
): Promise<void> {
  await revalidatePluginPath(root, canonicalPath, component)
  const [pathStats, openedStats] = await Promise.all([
    stat(canonicalPath, { bigint: true }),
    opened.stat({ bigint: true }),
  ])
  if (!pathStats.isFile() || !openedStats.isFile()) {
    throw pathError(
      'path-unresolvable',
      root,
      canonicalPath,
      component,
      `is not a regular file: ${canonicalPath}`,
    )
  }
  if (
    pathStats.dev !== openedStats.dev ||
    pathStats.ino !== openedStats.ino
  ) {
    throw pathError(
      'path-raced',
      root,
      canonicalPath,
      component,
      `path no longer names the opened file: ${canonicalPath}`,
    )
  }
}

async function openStablePluginDestination(
  root: string,
  canonicalPath: string,
  component: string,
  options: Pick<CanonicalPluginWriteOptions, 'exclusive' | 'mode'> = {},
) {
  const safePath = await resolveInternalPluginPath(root, canonicalPath, {
    mustExist: false,
    rejectSymlinks: true,
    rejectRoot: true,
    component,
  })
  if (!samePath(safePath, canonicalPath)) {
    throw pathError(
      'path-raced',
      root,
      canonicalPath,
      component,
      `changed before write: ${canonicalPath} -> ${safePath}`,
    )
  }

  await revalidatePluginPath(root, resolve(canonicalPath, '..'), component)
  const noFollow = process.platform === 'win32' ? 0 : constants.O_NOFOLLOW
  const exclusive = options.exclusive
    ? constants.O_EXCL
    : constants.O_TRUNC
  const handle = await open(
    canonicalPath,
    constants.O_WRONLY | constants.O_CREAT | exclusive | noFollow,
    options.mode,
  )
  try {
    await assertOpenPathIdentity(root, canonicalPath, component, handle)
    return handle
  } catch (error) {
    await handle.close()
    throw error
  }
}

/** Read a previously canonicalized plugin file without reopening a symlink alias. */
export async function readCanonicalPluginTextFile(
  root: string,
  canonicalPath: string,
  component = 'plugin file',
  options: { maxBytes?: number } = {},
): Promise<string> {
  const handle = await openStablePluginFile(root, canonicalPath, component)
  try {
    const maxBytes = options.maxBytes
    let content: string
    if (maxBytes === undefined) {
      content = await handle.readFile({ encoding: 'utf-8' })
    } else {
      if (!Number.isSafeInteger(maxBytes) || maxBytes < 0) {
        throw new Error(`${component} maxBytes must be a non-negative safe integer`)
      }
      const initial = await handle.stat()
      if (initial.size > maxBytes) {
        throw new Error(
          `${component} exceeds ${maxBytes} bytes: ${canonicalPath}`,
        )
      }
      // Read through the already validated descriptor and reserve one sentinel
      // byte. The pre-read stat rejects ordinary oversized files cheaply; the
      // sentinel also catches a same-inode append between stat and read without
      // ever allocating based on attacker-controlled file size.
      const bounded = Buffer.allocUnsafe(maxBytes + 1)
      let offset = 0
      while (offset <= maxBytes) {
        const { bytesRead } = await handle.read(
          bounded,
          offset,
          bounded.length - offset,
          null,
        )
        if (bytesRead === 0) break
        offset += bytesRead
        if (offset > maxBytes) {
          throw new Error(
            `${component} exceeds ${maxBytes} bytes: ${canonicalPath}`,
          )
        }
      }
      content = bounded.subarray(0, offset).toString('utf-8')
    }
    await revalidatePluginPath(root, canonicalPath, component)
    return content
  } finally {
    await handle.close()
  }
}

/** Byte equivalent used for ZIP/MCPB inputs. */
export async function readCanonicalPluginFileBytes(
  root: string,
  canonicalPath: string,
  component = 'plugin file',
): Promise<Buffer> {
  const handle = await openStablePluginFile(root, canonicalPath, component)
  try {
    const content = await handle.readFile()
    await revalidatePluginPath(root, canonicalPath, component)
    return content
  } finally {
    await handle.close()
  }
}

/**
 * Write through an opened regular-file descriptor, rejecting pre-existing
 * symlinks and verifying that the pathname still names the opened inode.
 */
export async function writeCanonicalPluginFile(
  root: string,
  canonicalPath: string,
  data: string | Uint8Array,
  component = 'plugin file',
  options: CanonicalPluginWriteOptions = {},
): Promise<void> {
  const handle = await openStablePluginDestination(
    root,
    canonicalPath,
    component,
    options,
  )
  try {
    await handle.writeFile(data)
    if (options.mode !== undefined) {
      const applyMode = handle.chmod(options.mode)
      if (options.bestEffortMode) {
        await applyMode.catch(() => {})
      } else {
        await applyMode
      }
    }
    await assertOpenPathIdentity(root, canonicalPath, component, handle)
  } finally {
    await handle.close()
  }
}

/** Copy a canonical source file into a new, link-free cache destination. */
export async function copyCanonicalPluginFile(
  sourceRoot: string,
  canonicalSourcePath: string,
  destinationRoot: string,
  canonicalDestinationPath: string,
  component = 'plugin cache file',
): Promise<void> {
  const source = await openStablePluginFile(
    sourceRoot,
    canonicalSourcePath,
    `${component} source`,
  )
  let destination: Awaited<ReturnType<typeof open>> | undefined
  try {
    const sourceStats = await source.stat({ bigint: true })
    const sourceMode = Number(sourceStats.mode & 0o777n)
    destination = await openStablePluginDestination(
      destinationRoot,
      canonicalDestinationPath,
      `${component} destination`,
      { exclusive: true, mode: sourceMode },
    )

    const buffer = Buffer.allocUnsafe(1024 * 1024)
    let position = 0
    for (;;) {
      const { bytesRead } = await source.read(
        buffer,
        0,
        buffer.length,
        position,
      )
      if (bytesRead === 0) break
      let written = 0
      while (written < bytesRead) {
        const result = await destination.write(
          buffer,
          written,
          bytesRead - written,
          position + written,
        )
        if (result.bytesWritten === 0) {
          throw new Error(
            `Plugin cache write made no progress: ${canonicalDestinationPath}`,
          )
        }
        written += result.bytesWritten
      }
      position += bytesRead
    }

    // Apply the source mode through the pinned descriptor. Creation mode is
    // still subject to umask, and chmod(path) would reintroduce a link race.
    await destination.chmod(sourceMode).catch(() => {})

    await revalidatePluginPath(
      sourceRoot,
      canonicalSourcePath,
      `${component} source`,
    )
    await assertOpenPathIdentity(
      destinationRoot,
      canonicalDestinationPath,
      `${component} destination`,
      destination,
    )
  } finally {
    await destination?.close()
    await source.close()
  }
}
