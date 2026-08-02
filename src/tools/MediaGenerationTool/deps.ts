/**
 * Injectable dependencies for the media-generation tool. Every external
 * effect (SDK client, catalog read, model selection, disk write, network
 * fetch) is injectable so tests pass fakes instead of `mock.module`, which
 * would replace shared modules process-wide and leak across the single-process
 * test run.
 */

import { Buffer } from 'node:buffer'
import { randomUUID } from 'node:crypto'
import { mkdir, writeFile } from 'node:fs/promises'
import { homedir } from 'node:os'
import { join } from 'node:path'
import { getAcosmiClient } from '../../services/acosmi/client.js'
import {
  getCachedImageGenerationModels,
  getCachedVideoGenerationModels,
} from '../../utils/model/modelCapabilities.js'
import {
  selectImageGenerationModel,
  selectVideoGenerationModel,
} from '../../utils/model/selectMediaGenerationModel.js'

/** Bytes + a media kind → save to disk; returns the absolute path written. */
export type SaveMediaToDisk = (args: {
  bytes: Uint8Array
  /** File extension WITHOUT the leading dot (e.g. "png", "mp4"). */
  ext: string
}) => Promise<string>

/**
 * Default disk writer: saves under `<os.homedir()>/.crabcode/generated-media/
 * <uuid>.<ext>`, creating the directory if needed. The stable home-relative
 * location keeps generated files outside arbitrary project worktrees and makes
 * their absolute paths reusable across TUI turns.
 */
export const defaultSaveMediaToDisk: SaveMediaToDisk = async ({ bytes, ext }) => {
  const dir = join(homedir(), '.crabcode', 'generated-media')
  await mkdir(dir, { recursive: true })
  const path = join(dir, `${randomUUID()}.${ext}`)
  await writeFile(path, Buffer.from(bytes))
  return path
}

export type MediaGenerationDeps = {
  getAcosmiClient: typeof getAcosmiClient
  /** Image-endpoint candidates; never the managed-chat catalog projection. */
  getImageGenerationModelCandidates: typeof getCachedImageGenerationModels
  /** Video-endpoint candidates; never the managed-chat catalog projection. */
  getVideoGenerationModelCandidates: typeof getCachedVideoGenerationModels
  selectImageGenerationModel: typeof selectImageGenerationModel
  selectVideoGenerationModel: typeof selectVideoGenerationModel
  saveMediaToDisk: SaveMediaToDisk
  /** Injectable for tests; production uses the global fetch. Minimal shape so
   *  test fakes don't have to implement the full `fetch` interface. */
  fetch: (url: string, init?: { signal?: AbortSignal }) => Promise<{
    ok: boolean
    status: number
    headers?: { get(name: string): string | null }
    body?: {
      getReader(): {
        read(): Promise<{ done: boolean; value?: Uint8Array }>
        cancel(reason?: unknown): Promise<void>
      }
    } | null
    arrayBuffer: () => Promise<ArrayBuffer>
  }>
}

export const DEFAULT_MEDIA_GENERATION_DEPS: MediaGenerationDeps = {
  getAcosmiClient,
  getImageGenerationModelCandidates: getCachedImageGenerationModels,
  getVideoGenerationModelCandidates: getCachedVideoGenerationModels,
  selectImageGenerationModel,
  selectVideoGenerationModel,
  saveMediaToDisk: defaultSaveMediaToDisk,
  fetch: (url, init) => globalThis.fetch(url, init),
}
