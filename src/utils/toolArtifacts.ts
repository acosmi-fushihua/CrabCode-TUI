import { createHash, randomUUID } from 'node:crypto'
import { constants as fsConstants } from 'node:fs'
import {
  chmod,
  lstat,
  mkdir,
  open,
  readFile,
  readdir,
  realpath,
  rename,
  rm,
  stat,
  writeFile,
} from 'node:fs/promises'
import { basename, dirname, extname, isAbsolute, join, resolve } from 'node:path'
import {
  MAX_TOOL_ARTIFACT_BYTES_BY_KIND,
  MAX_TOOL_ARTIFACT_CANDIDATES_SCANNED,
  MAX_TOOL_ARTIFACTS_PER_RESULT,
  MAX_TOTAL_TOOL_ARTIFACT_BYTES_PER_RESULT,
  type ToolArtifact,
  type ToolArtifactCandidate,
  type ToolArtifactKind,
} from '../types/toolArtifact.js'
export {
  MAX_TOOL_ARTIFACT_BYTES_BY_KIND,
  MAX_TOOL_ARTIFACT_CANDIDATES_SCANNED,
  MAX_TOOL_ARTIFACTS_PER_RESULT,
  MAX_TOTAL_TOOL_ARTIFACT_BYTES_PER_RESULT,
} from '../types/toolArtifact.js'
/*
 * Keep the validation implementation here while the dependency-free limits
 * live beside the wire types. Media producers can share limits without
 * importing this module's MCP/image-processing graph and creating cycles.
 */
import { parsePluginMcpRuntimeName } from '../services/mcp/pluginMcpIdentity.js'
import {
  isImageWithinArtifactDecodeBudget,
  sniffSupportedImageMimeBytes,
} from './imageArtifactSafety.js'
import { maybeResizeAndDownsampleImageBuffer } from './imageResizer.js'
import { getCrabCodeConfigHomeDir } from './envUtils.js'

export const MCP_ARTIFACT_META_KEY = 'com.acosmi.crabcode/artifacts'

const MIME_BY_EXT: Readonly<Record<string, string>> = {
  '.png': 'image/png',
  '.jpg': 'image/jpeg',
  '.jpeg': 'image/jpeg',
  '.gif': 'image/gif',
  '.webp': 'image/webp',
  '.mp4': 'video/mp4',
  '.webm': 'video/webm',
  '.mp3': 'audio/mpeg',
  '.wav': 'audio/wav',
  '.ogg': 'audio/ogg',
  '.pdf': 'application/pdf',
  '.zip': 'application/zip',
}

const MAX_ARTIFACT_ID_LENGTH = 256
const MAX_ARTIFACT_DISPLAY_NAME_LENGTH = 1_024
const MAX_ARTIFACT_MIME_LENGTH = 256
const MAX_ARTIFACT_PATH_LENGTH = 4_096
const MAX_ARTIFACT_URI_LENGTH = 8_192

const MAGIC_SNIFFED_MIMES = new Set(Object.values(MIME_BY_EXT))
const TOOL_ARTIFACT_CACHE_TTL_MS = 7 * 24 * 60 * 60 * 1000
const TOOL_ARTIFACT_CACHE_PRUNE_INTERVAL_MS = 60 * 60 * 1000
export const TOOL_ARTIFACT_CACHE_MAX_BYTES = 2 * 1024 * 1024 * 1024
export const TOOL_ARTIFACT_CACHE_MAX_ENTRIES = 4_096
const COPY_BUFFER_BYTES = 64 * 1024
const CACHE_LOCK_FILENAME = '.artifact-cache.lock'
const CACHE_LOCK_STALE_MS = 10 * 60 * 1000
const CACHE_LOCK_ATTEMPTS = 100
const CACHE_LOCK_RETRY_MS = 25
const UUID_V4_SOURCE = '[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}'
const GENERATED_CACHE_FILE_RE = new RegExp(`^${UUID_V4_SOURCE}\\.(?:png|jpg|jpeg|gif|webp|mp4|webm|mp3|wav|ogg|pdf|zip|bin)$`)
const GENERATED_PARTIAL_FILE_RE = /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}\.partial$/

let toolArtifactCacheRootOverride: string | null = null
let toolArtifactCacheMaxBytesOverride: number | null = null
let toolArtifactCacheMaxEntriesOverride: number | null = null
let lastToolArtifactCachePruneAt = 0
let toolArtifactCacheMutation = Promise.resolve()

async function acquireToolArtifactCacheMutation(): Promise<() => void> {
  const previous = toolArtifactCacheMutation
  let release!: () => void
  toolArtifactCacheMutation = new Promise<void>(resolveLock => {
    release = resolveLock
  })
  await previous
  return release
}

async function acquireCrossProcessCacheLock(root: string): Promise<() => Promise<void>> {
  const lockPath = join(root, CACHE_LOCK_FILENAME)
  for (let attempt = 0; attempt < CACHE_LOCK_ATTEMPTS; attempt += 1) {
    try {
      const handle = await open(lockPath, 'wx', 0o600)
      try {
        await handle.writeFile(`${process.pid}\n${Date.now()}\n`)
      } catch (error) {
        await handle.close().catch(() => undefined)
        await rm(lockPath, { force: true }).catch(() => undefined)
        throw error
      }
      return async () => {
        await handle.close().catch(() => undefined)
        await rm(lockPath, { force: true }).catch(() => undefined)
      }
    } catch (error) {
      const code = error && typeof error === 'object' && 'code' in error
        ? String(error.code)
        : ''
      if (code !== 'EEXIST') throw error
      try {
        const lockInfo = await stat(lockPath)
        if (Date.now() - lockInfo.mtimeMs > CACHE_LOCK_STALE_MS) {
          await rm(lockPath, { force: true })
          continue
        }
      } catch {
        continue
      }
      await new Promise(resolveRetry => setTimeout(resolveRetry, CACHE_LOCK_RETRY_MS))
    }
  }
  throw new Error('Tool artifact cache is busy')
}

/**
 * Tauri's asset protocol is intentionally scoped to `$HOME/**`. Every local
 * artifact is therefore frozen into this host-owned cache after validation;
 * widening the protocol to arbitrary absolute paths would turn a forged tool
 * result into a local-file read primitive.
 */
export function getToolArtifactCacheRoot(): string {
  return toolArtifactCacheRootOverride ?? join(getCrabCodeConfigHomeDir(), 'artifact-cache')
}

export function setToolArtifactCacheRootForTests(root: string | null): void {
  toolArtifactCacheRootOverride = root
  lastToolArtifactCachePruneAt = 0
}

export function setToolArtifactCacheMaxBytesForTests(maxBytes: number | null): void {
  toolArtifactCacheMaxBytesOverride = maxBytes
  lastToolArtifactCachePruneAt = 0
}

export function setToolArtifactCacheMaxEntriesForTests(maxEntries: number | null): void {
  toolArtifactCacheMaxEntriesOverride = maxEntries
  lastToolArtifactCachePruneAt = 0
}

export function isHtmlVideoRuntimePathCompatibilityProducer(
  mcpInfo: { serverName: string; toolName: string } | undefined,
): boolean {
  if (!mcpInfo) return false
  const identity = parsePluginMcpRuntimeName(mcpInfo.serverName)
  return identity?.pluginName === 'crabcode-html-video' &&
    identity.serverName === 'html-video' &&
    /^render[_-]?frames$/i.test(mcpInfo.toolName)
}

export function isHostAuthorizedHtmlVideoRuntimePathProducer(
  mcpInfo: { serverName: string; toolName: string } | undefined,
  clients: ReadonlyArray<{
    name: string
    type: string
    runtimePathAuthority?: string
  }>,
): boolean {
  return isHtmlVideoRuntimePathCompatibilityProducer(mcpInfo) &&
    clients.some(client =>
      client.type === 'connected' &&
      client.name === mcpInfo?.serverName &&
      client.runtimePathAuthority === 'installed-html-video',
    )
}

async function ensureToolArtifactCacheRoot(): Promise<string> {
  const configuredRoot = resolve(getToolArtifactCacheRoot())
  if (toolArtifactCacheRootOverride === null) {
    const expectedParent = resolve(getCrabCodeConfigHomeDir())
    const expectedRoot = join(expectedParent, 'artifact-cache')
    if (configuredRoot !== expectedRoot) {
      throw new Error('Tool artifact cache root does not match the fixed application path')
    }
    try {
      const parentInfo = await lstat(expectedParent)
      if (!parentInfo.isDirectory() || parentInfo.isSymbolicLink()) {
        throw new Error('Tool artifact cache parent must be a real directory')
      }
    } catch (error) {
      if ((error as NodeJS.ErrnoException).code !== 'ENOENT') throw error
      await mkdir(expectedParent, { recursive: true, mode: 0o700 })
    }
  } else {
    await mkdir(dirname(configuredRoot), { recursive: true, mode: 0o700 })
  }
  try {
    const rootInfo = await lstat(configuredRoot)
    if (!rootInfo.isDirectory() || rootInfo.isSymbolicLink()) {
      throw new Error('Tool artifact cache root must be a real directory')
    }
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code !== 'ENOENT') throw error
    await mkdir(configuredRoot, { mode: 0o700 })
  }
  await chmod(configuredRoot, 0o700)
  const resolvedRoot = await realpath(configuredRoot)
  if (toolArtifactCacheRootOverride === null) {
    const resolvedParent = await realpath(resolve(getCrabCodeConfigHomeDir()))
    if (
      resolvedRoot !== join(resolvedParent, 'artifact-cache') ||
      dirname(resolvedRoot) !== resolvedParent
    ) {
      throw new Error('Tool artifact cache must be the fixed non-symlink application directory')
    }
  }
  return resolvedRoot
}

async function pruneToolArtifactCache(
  root: string,
  incomingBytes: number,
  incomingEntries: number,
): Promise<boolean> {
  const maxBytes = toolArtifactCacheMaxBytesOverride ?? TOOL_ARTIFACT_CACHE_MAX_BYTES
  const maxEntries = toolArtifactCacheMaxEntriesOverride ?? TOOL_ARTIFACT_CACHE_MAX_ENTRIES
  if (!Number.isFinite(incomingBytes) || incomingBytes < 0 || incomingBytes > maxBytes) {
    return false
  }
  const now = Date.now()
  const pruneExpired = now - lastToolArtifactCachePruneAt >= TOOL_ARTIFACT_CACHE_PRUNE_INTERVAL_MS
  if (pruneExpired) lastToolArtifactCachePruneAt = now
  let entries
  try {
    entries = await readdir(root, { withFileTypes: true })
  } catch {
    return false
  }
  let retainedBytes = 0
  let retainedEntries = 0
  for (const entry of entries) {
    if (entry.name === CACHE_LOCK_FILENAME) continue
    const generatedFile = GENERATED_CACHE_FILE_RE.test(entry.name)
    const generatedPartial = GENERATED_PARTIAL_FILE_RE.test(entry.name)
    // Cleanup authority only covers files created by freezeLocalArtifact.
    // Foreign entries and symlinks are preserved untouched and excluded from
    // the quota: they are not ours to evict, and counting them would let a
    // stray OS junk file (.DS_Store, desktop.ini, AV marker) or a planted
    // oversized file starve every future artifact — the pre-fix behavior this
    // replaces wedged the cache closed forever on any such entry.
    if ((!generatedFile && !generatedPartial) || !entry.isFile() || entry.isSymbolicLink()) {
      continue
    }
    const path = join(root, entry.name)
    try {
      const info = await lstat(path)
      if (!info.isFile() || info.isSymbolicLink()) continue
      if (pruneExpired && now - info.mtimeMs > TOOL_ARTIFACT_CACHE_TTL_MS) {
        await rm(path, { force: true })
        continue
      }
      retainedBytes += info.size
      retainedEntries += 1
    } catch {
      // The entry vanished under the cross-process lock (or is unreadable).
      // Treat it as absent for quota instead of wedging the cache closed.
      continue
    }
  }
  // Without an owner/refcount index, evicting a non-expired file can break a
  // persisted transcript. Preserve every live artifact and reject the new
  // candidate when the global quota has no room; TTL cleanup is the only
  // automatic deletion policy at this layer.
  return retainedBytes + incomingBytes <= maxBytes &&
    retainedEntries + incomingEntries <= maxEntries
}

function extensionForArtifactMime(mimeType: string): string {
  for (const [ext, knownMime] of Object.entries(MIME_BY_EXT)) {
    if (knownMime === mimeType) return ext.slice(1)
  }
  return 'bin'
}

function isMimeCompatibleWithKind(kind: ToolArtifactKind, mimeType: string): boolean {
  switch (kind) {
    case 'image':
      return /^image\/(?:png|jpeg|gif|webp)$/i.test(mimeType)
    case 'video':
      return /^video\/(?:mp4|webm)$/i.test(mimeType)
    case 'audio':
      return /^audio\/(?:mpeg|wav|ogg)$/i.test(mimeType)
    case 'archive':
      return mimeType === 'application/zip'
    case 'document':
      return !/^(?:image|video|audio)\//i.test(mimeType)
    case 'other':
      return true
  }
}

type FrozenLocalArtifact = {
  path: string
  byteSize: number
  sha256: string
  mimeType: string
}

async function freezeLocalArtifact(
  sourcePath: string,
  candidate: ToolArtifactCandidate,
): Promise<FrozenLocalArtifact | null> {
  const maxBytes = MAX_TOOL_ARTIFACT_BYTES_BY_KIND[candidate.kind]
  const noFollow = typeof fsConstants.O_NOFOLLOW === 'number'
    ? fsConstants.O_NOFOLLOW
    : 0
  const source = await open(sourcePath, fsConstants.O_RDONLY | noFollow)
  let partialPath: string | null = null
  let finalPath: string | null = null
  let committed = false
  let releaseCacheMutation: (() => void) | null = null
  let releaseCrossProcessLock: (() => Promise<void>) | null = null
  try {
    const before = await source.stat()
    if (!before.isFile() || before.size > maxBytes) return null

    releaseCacheMutation = await acquireToolArtifactCacheMutation()
    const cacheRoot = await ensureToolArtifactCacheRoot()
    releaseCrossProcessLock = await acquireCrossProcessCacheLock(cacheRoot)
    if (!await pruneToolArtifactCache(cacheRoot, before.size, 1)) return null
    partialPath = join(cacheRoot, `${randomUUID()}.partial`)
    const destination = await open(partialPath, 'wx', 0o600)
    const hash = createHash('sha256')
    let copied = 0
    try {
      const buffer = Buffer.allocUnsafe(COPY_BUFFER_BYTES)
      while (true) {
        const { bytesRead } = await source.read(buffer, 0, buffer.length, null)
        if (bytesRead === 0) break
        copied += bytesRead
        if (copied > maxBytes) return null
        const chunk = buffer.subarray(0, bytesRead)
        hash.update(chunk)
        let written = 0
        while (written < bytesRead) {
          const result = await destination.write(
            chunk,
            written,
            bytesRead - written,
            null,
          )
          if (result.bytesWritten === 0) return null
          written += result.bytesWritten
        }
      }
      await destination.sync()
    } finally {
      await destination.close()
    }

    const after = await source.stat()
    if (
      copied !== before.size ||
      after.size !== before.size ||
      after.dev !== before.dev ||
      after.ino !== before.ino ||
      after.mtimeMs !== before.mtimeMs ||
      after.ctimeMs !== before.ctimeMs
    ) return null

    let frozen = await stat(partialPath)
    if (!frozen.isFile() || frozen.size !== copied) return null
    const sourceSha256 = hash.digest('hex')
    if (
      candidate.sha256 !== undefined &&
      (!/^[a-f0-9]{64}$/i.test(candidate.sha256) ||
        candidate.sha256.toLowerCase() !== sourceSha256)
    ) return null

    let frozenMimeType = candidate.mimeType
    let frozenSha256 = sourceSha256
    if (candidate.kind === 'image') {
      const sourceBytes = await readFile(partialPath)
      if (
        sniffArtifactMimeBytes(sourceBytes.subarray(0, 32)) !== candidate.mimeType ||
        !isImageWithinArtifactDecodeBudget(sourceBytes, candidate.mimeType)
      ) return null
      const resized = await maybeResizeAndDownsampleImageBuffer(
        sourceBytes,
        sourceBytes.length,
        candidate.mimeType.split('/')[1] || 'png',
      )
      frozenMimeType = `image/${resized.mediaType}`.toLowerCase()
      if (!isMimeCompatibleWithKind('image', frozenMimeType)) return null
      if (!resized.buffer.equals(sourceBytes)) {
        await writeFile(partialPath, resized.buffer, { flag: 'w', mode: 0o600 })
        await chmod(partialPath, 0o600)
      }
      copied = resized.buffer.length
      if (copied > maxBytes) return null
      frozenSha256 = createHash('sha256').update(resized.buffer).digest('hex')
      frozen = await stat(partialPath)
      if (!frozen.isFile() || frozen.size !== copied) return null
    }

    if (MAGIC_SNIFFED_MIMES.has(frozenMimeType)) {
      const sniffedMime = await sniffArtifactMime(partialPath)
      if (sniffedMime !== frozenMimeType) return null
    }

    finalPath = join(
      cacheRoot,
      `${randomUUID()}.${extensionForArtifactMime(frozenMimeType)}`,
    )
    await rename(partialPath, finalPath)
    partialPath = null
    await chmod(finalPath, 0o600)
    // Reconcile races between concurrent producers. A quota race rejects this
    // just-written candidate; it never evicts a file referenced by history.
    if (!await pruneToolArtifactCache(cacheRoot, 0, 0)) {
      await rm(finalPath, { force: true })
      return null
    }
    try {
      const finalInfo = await stat(finalPath)
      if (!finalInfo.isFile() || finalInfo.size !== copied) return null
    } catch {
      return null
    }
    const resolvedFinalPath = await realpath(finalPath)
    committed = true
    return {
      path: resolvedFinalPath,
      byteSize: copied,
      sha256: frozenSha256,
      mimeType: frozenMimeType,
    }
  } finally {
    await source.close()
    if (partialPath !== null) await rm(partialPath, { force: true })
    // A failure after rename (chmod/stat/realpath/quota reconciliation) must
    // not strand an unreferenced final artifact in the cache.
    if (!committed && finalPath !== null) await rm(finalPath, { force: true })
    await releaseCrossProcessLock?.()
    releaseCacheMutation?.()
  }
}

function sniffArtifactMimeBytes(bytes: Buffer): string | null {
    const imageMime = sniffSupportedImageMimeBytes(bytes)
    if (imageMime !== null) return imageMime
    const ascii = (start: number, end: number) => bytes.subarray(start, end).toString('ascii')
    if (bytes.length >= 12 && ascii(4, 8) === 'ftyp') return 'video/mp4'
    if (bytes.length >= 4 && bytes.subarray(0, 4).equals(Buffer.from([0x1a, 0x45, 0xdf, 0xa3]))) return 'video/webm'
    if (bytes.length >= 12 && ascii(0, 4) === 'RIFF' && ascii(8, 12) === 'WAVE') return 'audio/wav'
    if (bytes.length >= 4 && ascii(0, 4) === 'OggS') return 'audio/ogg'
    if (bytes.length >= 3 && ascii(0, 3) === 'ID3') return 'audio/mpeg'
    if (bytes.length >= 2 && bytes[0] === 0xff && (bytes[1]! & 0xe0) === 0xe0) return 'audio/mpeg'
    if (bytes.length >= 5 && ascii(0, 5) === '%PDF-') return 'application/pdf'
    if (
      bytes.length >= 4 && bytes[0] === 0x50 && bytes[1] === 0x4b &&
      ((bytes[2] === 0x03 && bytes[3] === 0x04) ||
        (bytes[2] === 0x05 && bytes[3] === 0x06) ||
        (bytes[2] === 0x07 && bytes[3] === 0x08))
    ) return 'application/zip'
    return null
}

async function sniffArtifactMime(path: string): Promise<string | null> {
  const handle = await open(path, 'r')
  try {
    const head = Buffer.alloc(32)
    const { bytesRead } = await handle.read(head, 0, head.length, 0)
    return sniffArtifactMimeBytes(head.subarray(0, bytesRead))
  } finally {
    await handle.close()
  }
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === 'object' && !Array.isArray(value)
}

function candidateFromUnknown(value: unknown): ToolArtifactCandidate | null {
  if (!isRecord(value) || !isRecord(value.location)) return null
  const kind = value.kind
  if (
    kind !== 'image' &&
    kind !== 'video' &&
    kind !== 'audio' &&
    kind !== 'document' &&
    kind !== 'archive' &&
    kind !== 'other'
  ) return null
  if (
    typeof value.id !== 'string' ||
    typeof value.mimeType !== 'string' ||
    typeof value.displayName !== 'string'
  ) return null
  if (
    value.id.length > MAX_ARTIFACT_ID_LENGTH ||
    value.mimeType.length > MAX_ARTIFACT_MIME_LENGTH ||
    value.displayName.length > MAX_ARTIFACT_DISPLAY_NAME_LENGTH
  ) return null
  let location: ToolArtifactCandidate['location']
  if (
    value.location.type === 'runtimePath' &&
    typeof value.location.path === 'string' &&
    value.location.path.length <= MAX_ARTIFACT_PATH_LENGTH
  ) {
    location = { type: 'runtimePath', path: value.location.path }
  } else if (
    value.location.type === 'externalUri' &&
    typeof value.location.uri === 'string' &&
    value.location.uri.length <= MAX_ARTIFACT_URI_LENGTH
  ) {
    location = { type: 'externalUri', uri: value.location.uri }
  } else {
    return null
  }
  return {
    id: value.id,
    kind,
    mimeType: value.mimeType,
    displayName: value.displayName,
    location,
    ...(typeof value.byteSize === 'number' ? { byteSize: value.byteSize } : {}),
    ...(typeof value.sha256 === 'string' ? { sha256: value.sha256 } : {}),
  }
}

export function extractMcpArtifactCandidates(
  wireToolName: string,
  mcpMeta: {
    _meta?: Record<string, unknown>
    structuredContent?: Record<string, unknown>
  } | undefined,
  mcpInfo?: { serverName: string; toolName: string },
  trust?: { runtimePathAllowed: boolean },
): ToolArtifactCandidate[] {
  if (!mcpMeta) return []
  const declared = mcpMeta._meta?.[MCP_ARTIFACT_META_KEY]
  const candidates = (Array.isArray(declared)
    ? declared
        .slice(0, MAX_TOOL_ARTIFACT_CANDIDATES_SCANNED)
        .map(candidateFromUnknown)
        .filter((v): v is ToolArtifactCandidate => v !== null)
    : []).filter(candidate =>
      candidate.location.type !== 'runtimePath' || trust?.runtimePathAllowed === true,
    )
  if (candidates.length > 0) return candidates

  // Compatibility adapter for the bundled HTML-video MCP plugin. This is
  // deliberately tool-scoped; arbitrary structuredContent paths are never
  // promoted to local files.
  const pluginIdentity = mcpInfo
    ? parsePluginMcpRuntimeName(mcpInfo.serverName)
    : null
  const logicalToolName = mcpInfo?.toolName ?? wireToolName.split('__').at(-1) ?? wireToolName
  const logicalServers = mcpInfo
    ? [mcpInfo.serverName, pluginIdentity?.pluginName, pluginIdentity?.serverName]
    : [wireToolName]
  const isHtmlVideoRenderer =
    /^render[_-]?frames$/i.test(logicalToolName) &&
    logicalServers.some(value => typeof value === 'string' && /(?:crabcode[-_ ])?html[-_ ]?video/i.test(value))
  const structured = mcpMeta.structuredContent
  const compatibilityData = structured?.success === true && isRecord(structured.data)
    ? structured.data
    : null
  const outputPath = compatibilityData?.outputPath
  if (
    !isHtmlVideoRenderer ||
    typeof outputPath !== 'string' ||
    trust?.runtimePathAllowed !== true
  ) return []
  return [{
    id: 'rendered-video',
    kind: 'video',
    mimeType: 'video/mp4',
    displayName: basename(outputPath) || 'rendered-video.mp4',
    location: { type: 'runtimePath', path: outputPath },
    ...(typeof compatibilityData?.fileSize === 'number'
      ? { byteSize: compatibilityData.fileSize }
      : {}),
  }]
}

/**
 * Host validation boundary. Invalid, missing, oversized, active-content, or
 * duplicate candidates are dropped without affecting the core tool result.
 */
export async function normalizeToolArtifacts(
  candidates: readonly ToolArtifactCandidate[] | undefined,
  producerToolUseId: string,
): Promise<ToolArtifact[]> {
  if (!candidates?.length) return []
  const out: ToolArtifact[] = []
  const seenIds = new Set<string>()
  const seenLocations = new Set<string>()
  let acceptedBytes = 0
  for (const raw of candidates.slice(0, MAX_TOOL_ARTIFACT_CANDIDATES_SCANNED)) {
    if (out.length >= MAX_TOOL_ARTIFACTS_PER_RESULT) break
    const candidate = candidateFromUnknown(raw)
    if (!candidate || !candidate.id.trim() || seenIds.has(candidate.id)) continue
    const isActiveContent = /^(text\/html|image\/svg\+xml)$/i.test(candidate.mimeType)
    if (
      !candidate.mimeType ||
      (!isMimeCompatibleWithKind(candidate.kind, candidate.mimeType) &&
        !(candidate.location.type === 'externalUri' && isActiveContent))
    ) continue
    const declaredSize = candidate.byteSize
    if (
      declaredSize !== undefined &&
      (!Number.isFinite(declaredSize) || declaredSize < 0 ||
        declaredSize > MAX_TOOL_ARTIFACT_BYTES_BY_KIND[candidate.kind])
    ) continue
    if (candidate.location.type === 'externalUri') {
      let url: URL
      try {
        url = new URL(candidate.location.uri)
      } catch {
        continue
      }
      if (url.protocol !== 'https:' && url.protocol !== 'http:') continue
      const locationKey = `externalUri:${url.href}`
      if (seenLocations.has(locationKey)) continue
      const artifactBytes = candidate.byteSize ?? 0
      if (
        acceptedBytes + artifactBytes >
        MAX_TOTAL_TOOL_ARTIFACT_BYTES_PER_RESULT
      ) continue
      seenIds.add(candidate.id)
      seenLocations.add(locationKey)
      acceptedBytes += artifactBytes
      out.push({
        ...candidate,
        location: { type: 'externalUri', uri: url.href },
        producerToolUseId,
      })
      continue
    }
    // Active formats may be offered as an ordinary remote link, but local
    // bytes are never mounted into the WebView.
    if (isActiveContent) continue
    if (!isAbsolute(candidate.location.path)) continue
    try {
      const path = await realpath(candidate.location.path)
      const file = await stat(path)
      if (!file.isFile() || file.size > MAX_TOOL_ARTIFACT_BYTES_BY_KIND[candidate.kind]) continue
      if (
        acceptedBytes + file.size >
        MAX_TOTAL_TOOL_ARTIFACT_BYTES_PER_RESULT
      ) continue
      const expectedMime = MIME_BY_EXT[extname(path).toLowerCase()]
      if (expectedMime && expectedMime !== candidate.mimeType) continue
      const locationKey = `runtimePath:${path}`
      if (seenLocations.has(locationKey)) continue
      const frozen = await freezeLocalArtifact(path, candidate)
      if (frozen === null) continue
      seenIds.add(candidate.id)
      seenLocations.add(locationKey)
      acceptedBytes += frozen.byteSize
      out.push({
        ...candidate,
        displayName: candidate.displayName.trim() || basename(path),
        location: { type: 'runtimePath', path: frozen.path },
        mimeType: frozen.mimeType,
        byteSize: frozen.byteSize,
        sha256: frozen.sha256,
        producerToolUseId,
      })
    } catch {
      continue
    }
  }
  return out
}
