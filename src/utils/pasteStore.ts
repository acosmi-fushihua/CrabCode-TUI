import { createHash } from 'crypto'
import {
  lstat,
  mkdir,
  open,
  readdir,
  readFile,
  stat,
  unlink,
  writeFile,
} from 'fs/promises'
import { join } from 'path'
import { logForDebugging } from './debug.js'
import { getCrabCodeConfigHomeDir } from './envUtils.js'
import { isENOENT } from './errors.js'

const PASTE_STORE_DIR = 'paste-cache'

/**
 * Get the paste store directory (persistent across sessions).
 */
function getPasteStoreDir(): string {
  return join(getCrabCodeConfigHomeDir(), PASTE_STORE_DIR)
}

/**
 * Generate a hash for paste content to use as filename.
 * Exported so callers can get the hash synchronously before async storage.
 */
export function hashPastedText(content: string): string {
  return createHash('sha256').update(content).digest('hex').slice(0, 16)
}

/**
 * Get the file path for a paste by its content hash.
 */
function getPastePath(hash: string): string {
  return join(getPasteStoreDir(), `${hash}.txt`)
}

/**
 * Store pasted text content to disk.
 * The hash should be pre-computed with hashPastedText() so the caller
 * can use it immediately without waiting for the async disk write.
 */
export async function storePastedText(
  hash: string,
  content: string,
): Promise<void> {
  try {
    const dir = getPasteStoreDir()
    await mkdir(dir, { recursive: true })

    const pastePath = getPastePath(hash)

    // Content-addressable: same hash = same content, so overwriting is safe
    await writeFile(pastePath, content, { encoding: 'utf8', mode: 0o600 })
    logForDebugging(`Stored paste ${hash} to ${pastePath}`)
  } catch (error) {
    logForDebugging(`Failed to store paste: ${error}`)
  }
}

/**
 * Retrieve pasted text content by its hash.
 * Returns null if not found or on error.
 */
export async function retrievePastedText(hash: string): Promise<string | null> {
  try {
    const pastePath = getPastePath(hash)
    return await readFile(pastePath, { encoding: 'utf8' })
  } catch (error) {
    // ENOENT is expected when paste doesn't exist
    if (!isENOENT(error)) {
      logForDebugging(`Failed to retrieve paste ${hash}: ${error}`)
    }
    return null
  }
}

export type BoundedPastedTextRead =
  | { status: 'found'; content: string }
  | { status: 'missing' }
  | { status: 'too_large' }

/**
 * Read at most `maxBytes` from a paste-cache regular file.
 *
 * Composer-history pickers call this only after the user selects one entry.
 * The explicit byte ceiling prevents a damaged cache file from turning one
 * JSON-RPC response into an unbounded allocation. Symlinks and non-regular
 * files are not paste-cache objects and are never followed.
 */
export async function retrievePastedTextBounded(
  hash: string,
  maxBytes: number,
): Promise<BoundedPastedTextRead> {
  if (!Number.isSafeInteger(maxBytes) || maxBytes < 0) {
    throw new TypeError('maxBytes must be a non-negative safe integer')
  }

  const pastePath = getPastePath(hash)
  let fileHandle
  try {
    const pathMetadata = await lstat(pastePath)
    if (pathMetadata.isSymbolicLink() || !pathMetadata.isFile()) {
      logForDebugging(`Refusing non-regular paste cache object ${hash}`)
      return { status: 'missing' }
    }

    fileHandle = await open(pastePath, 'r')
    const openedMetadata = await fileHandle.stat()
    if (
      !openedMetadata.isFile() ||
      openedMetadata.dev !== pathMetadata.dev ||
      openedMetadata.ino !== pathMetadata.ino
    ) {
      logForDebugging(`Paste cache object changed while opening ${hash}`)
      return { status: 'missing' }
    }
    if (openedMetadata.size > maxBytes) {
      return { status: 'too_large' }
    }

    const buffer = Buffer.alloc(maxBytes + 1)
    let bytesReadTotal = 0
    while (bytesReadTotal < buffer.length) {
      const { bytesRead } = await fileHandle.read(
        buffer,
        bytesReadTotal,
        buffer.length - bytesReadTotal,
        bytesReadTotal,
      )
      if (bytesRead === 0) break
      bytesReadTotal += bytesRead
    }
    if (bytesReadTotal > maxBytes) {
      return { status: 'too_large' }
    }
    return {
      status: 'found',
      content: buffer.toString('utf8', 0, bytesReadTotal),
    }
  } catch (error) {
    if (!isENOENT(error)) {
      logForDebugging(`Failed to retrieve bounded paste ${hash}: ${error}`)
    }
    return { status: 'missing' }
  } finally {
    await fileHandle?.close()
  }
}

/**
 * Clean up old paste files that are no longer referenced.
 * This is a simple time-based cleanup - removes files older than cutoffDate.
 */
export async function cleanupOldPastes(cutoffDate: Date): Promise<void> {
  const pasteDir = getPasteStoreDir()

  let files
  try {
    files = await readdir(pasteDir)
  } catch {
    // Directory doesn't exist or can't be read - nothing to clean up
    return
  }

  const cutoffTime = cutoffDate.getTime()
  for (const file of files) {
    if (!file.endsWith('.txt')) {
      continue
    }

    const filePath = join(pasteDir, file)
    try {
      const stats = await stat(filePath)
      if (stats.mtimeMs < cutoffTime) {
        await unlink(filePath)
        logForDebugging(`Cleaned up old paste: ${filePath}`)
      }
    } catch {
      // Ignore errors for individual files
    }
  }
}
