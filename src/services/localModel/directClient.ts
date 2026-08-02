import {
  localModelByoAddHandler,
  localModelByoRemoveHandler,
  localModelCatalogReadHandler,
  localModelDownloadCancelHandler,
  localModelDownloadProgressHandler,
  localModelDownloadStartHandler,
  localModelInstallRemoveHandler,
  localModelServerStartHandler,
  localModelServerStatusHandler,
  localModelServerStopHandler,
  localModelSystemProfileReadHandler,
  type LocalModelRuntime,
  type WorkerLocalModelCatalogReadResult,
  type WorkerLocalModelDownloadResult,
  type WorkerLocalModelInstallRemoveResult,
  type WorkerLocalModelMergedCatalogEntry,
  type WorkerLocalModelServerResult,
  type WorkerLocalModelSystemProfileResult,
} from './authority.js'

export type LocalModelCatalogReadResponse =
  WorkerLocalModelCatalogReadResult
export type LocalModelCatalogReadParams = Record<string, never>
export type LocalModelSystemProfileReadResponse =
  WorkerLocalModelSystemProfileResult
export type LocalModelSystemProfileReadParams = Record<string, never>
export type LocalModelDownloadStartParams = { modelId: string }
export type LocalModelDownloadLocator = {
  downloadId?: string | null
  modelId?: string | null
}
export type LocalModelInstallRemoveParams = {
  modelId?: string | null
  modelPath?: string | null
  removeFiles: boolean
}
export type LocalModelServerStartParams = {
  modelId?: string | null
  modelPath?: string | null
  runtime?: LocalModelRuntime | null
  port?: number | null
  contextSize?: number | null
  gpuLayers?: number | null
}
export type LocalModelServerStopParams = {
  modelId?: string | null
  modelPath?: string | null
}
export type LocalModelByoAddParams = {
  ggufPath: string
  displayName?: string | null
}
export type LocalModelByoRemoveParams = { id: string }
export type LocalModelDownloadStartResponse =
  WorkerLocalModelDownloadResult
export type LocalModelDownloadProgressParams =
  LocalModelDownloadLocator
export type LocalModelDownloadProgressResponse =
  WorkerLocalModelDownloadResult
export type LocalModelDownloadCancelParams = LocalModelDownloadLocator
export type LocalModelDownloadCancelResponse =
  WorkerLocalModelDownloadResult
export type LocalModelInstallRemoveResponse =
  WorkerLocalModelInstallRemoveResult
export type LocalModelServerStartResponse = WorkerLocalModelServerResult
export type LocalModelServerStopResponse = WorkerLocalModelServerResult
export type LocalModelServerStatusParams = Record<string, never>
export type LocalModelServerStatusResponse = WorkerLocalModelServerResult
export type LocalModelByoAddResponse = {
  entry: WorkerLocalModelMergedCatalogEntry
}
export type LocalModelByoRemoveResponse = { removed: boolean }

export interface LocalModelDirectClient {
  catalogRead(): Promise<LocalModelCatalogReadResponse>
  systemProfileRead(): Promise<LocalModelSystemProfileReadResponse>
  downloadStart(
    params: LocalModelDownloadStartParams,
  ): Promise<WorkerLocalModelDownloadResult>
  downloadProgress(
    params: LocalModelDownloadLocator,
  ): Promise<WorkerLocalModelDownloadResult>
  downloadCancel(
    params: LocalModelDownloadLocator,
  ): Promise<WorkerLocalModelDownloadResult>
  installRemove(
    params: LocalModelInstallRemoveParams,
  ): Promise<WorkerLocalModelInstallRemoveResult>
  serverStart(
    params: LocalModelServerStartParams,
  ): Promise<WorkerLocalModelServerResult>
  serverStop(
    params?: LocalModelServerStopParams,
  ): Promise<WorkerLocalModelServerResult>
  serverStatus(): Promise<WorkerLocalModelServerResult>
  byoAdd(
    params: LocalModelByoAddParams,
  ): Promise<{ entry: WorkerLocalModelMergedCatalogEntry }>
  byoRemove(params: LocalModelByoRemoveParams): Promise<{ removed: boolean }>
}

/**
 * In-process client over the local-model authority. This is an ordinary
 * function boundary, not a remote transport.
 */
class DefaultLocalModelDirectClient implements LocalModelDirectClient {
  catalogRead(): Promise<LocalModelCatalogReadResponse> {
    return localModelCatalogReadHandler({}, undefined)
  }

  systemProfileRead(): Promise<LocalModelSystemProfileReadResponse> {
    return localModelSystemProfileReadHandler({}, undefined)
  }

  downloadStart(
    params: LocalModelDownloadStartParams,
  ): Promise<WorkerLocalModelDownloadResult> {
    return localModelDownloadStartHandler(params, undefined)
  }

  downloadProgress(
    params: LocalModelDownloadLocator,
  ): Promise<WorkerLocalModelDownloadResult> {
    return localModelDownloadProgressHandler(params, undefined)
  }

  downloadCancel(
    params: LocalModelDownloadLocator,
  ): Promise<WorkerLocalModelDownloadResult> {
    return localModelDownloadCancelHandler(params, undefined)
  }

  installRemove(
    params: LocalModelInstallRemoveParams,
  ): Promise<WorkerLocalModelInstallRemoveResult> {
    return localModelInstallRemoveHandler(params, undefined)
  }

  serverStart(
    params: LocalModelServerStartParams,
  ): Promise<WorkerLocalModelServerResult> {
    return localModelServerStartHandler(params, undefined)
  }

  serverStop(
    params: LocalModelServerStopParams = {},
  ): Promise<WorkerLocalModelServerResult> {
    return localModelServerStopHandler(params, undefined)
  }

  serverStatus(): Promise<WorkerLocalModelServerResult> {
    return localModelServerStatusHandler({}, undefined)
  }

  byoAdd(
    params: LocalModelByoAddParams,
  ): Promise<{ entry: WorkerLocalModelMergedCatalogEntry }> {
    return localModelByoAddHandler(params, undefined)
  }

  byoRemove(
    params: LocalModelByoRemoveParams,
  ): Promise<{ removed: boolean }> {
    return localModelByoRemoveHandler(params, undefined)
  }
}

export function createDefaultLocalModelDirectClient(): LocalModelDirectClient {
  return new DefaultLocalModelDirectClient()
}

/** Compatibility name for renderer components; transport remains direct. */
export const createDefaultLocalModelTuiClient =
  createDefaultLocalModelDirectClient
export type LocalModelTuiClient = LocalModelDirectClient

/** Best-effort logout cleanup without spawning or connecting any daemon. */
export async function stopLocalModelServerOnLogoutDirect(): Promise<void> {
  try {
    await localModelServerStopHandler({}, undefined)
  } catch {
    // Logout remains authoritative even when no local server is present or
    // shutdown observation fails; inference entitlement is checked per turn.
  }
}
