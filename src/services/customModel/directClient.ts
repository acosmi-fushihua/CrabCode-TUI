import {
  mutateDirectCustomModels,
  readDirectCustomModels,
  type CustomModelEntry,
  type CustomModelEntryInput,
  type DirectCustomModelMutationResult,
} from './registryAuthority.js'
import {
  customModelTestConnectionHandler,
  type CustomModelTestConnectionParams,
  type CustomModelTestConnectionResponse,
} from './testConnectionAuthority.js'

export type {
  CustomModelEntry,
  CustomModelEntryInput,
  CustomModelTestConnectionParams,
  CustomModelTestConnectionResponse,
}

export interface CustomModelDirectClient {
  list(): Promise<CustomModelEntry[]>
  add(
    input: CustomModelEntryInput,
  ): Promise<DirectCustomModelMutationResult>
  update(
    id: string,
    input: CustomModelEntryInput,
  ): Promise<DirectCustomModelMutationResult>
  remove(id: string): Promise<DirectCustomModelMutationResult>
  toggle(
    id: string,
    enabled: boolean,
  ): Promise<DirectCustomModelMutationResult>
  testConnection(
    params: CustomModelTestConnectionParams,
  ): Promise<CustomModelTestConnectionResponse>
}

class DefaultCustomModelDirectClient
  implements CustomModelDirectClient
{
  list(): Promise<CustomModelEntry[]> {
    return readDirectCustomModels()
  }

  add(
    input: CustomModelEntryInput,
  ): Promise<DirectCustomModelMutationResult> {
    if (!input.apiKey?.trim()) {
      return Promise.reject(
        new Error(
          'add custom model: apiKey is required for a new entry',
        ),
      )
    }
    return mutateDirectCustomModels({
      type: 'addCustomModel',
      ...input,
      apiKey: input.apiKey.trim(),
    })
  }

  update(
    id: string,
    input: CustomModelEntryInput,
  ): Promise<DirectCustomModelMutationResult> {
    return mutateDirectCustomModels({
      type: 'updateCustomModel',
      id,
      ...input,
      ...(input.apiKey?.trim()
        ? { apiKey: input.apiKey.trim() }
        : { apiKey: undefined }),
    })
  }

  remove(id: string): Promise<DirectCustomModelMutationResult> {
    return mutateDirectCustomModels({
      type: 'removeCustomModel',
      id,
    })
  }

  toggle(
    id: string,
    enabled: boolean,
  ): Promise<DirectCustomModelMutationResult> {
    return mutateDirectCustomModels({
      type: 'toggleCustomModel',
      id,
      enabled,
    })
  }

  testConnection(
    params: CustomModelTestConnectionParams,
  ): Promise<CustomModelTestConnectionResponse> {
    return customModelTestConnectionHandler(params, undefined)
  }
}

export function createDefaultCustomModelDirectClient(): CustomModelDirectClient {
  return new DefaultCustomModelDirectClient()
}

/** Compatibility names for renderer components; transport remains direct. */
export type CustomModelTuiClient = CustomModelDirectClient
export const createDefaultCustomModelTuiClient =
  createDefaultCustomModelDirectClient
