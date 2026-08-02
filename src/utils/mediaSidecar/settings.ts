import { getSettingsForSource } from '../settings/settings.js'

const FALSY = /^(0|false|no|off)$/i

export const MEDIA_SIDECAR_ENV = 'CRABCODE_MEDIA_SIDECAR'
export const MEDIA_SIDECAR_DISK_CACHE_ENV = 'CRABCODE_MEDIA_SIDECAR_DISK_CACHE'

export type MediaSidecarConsentBinding = {
  provider: string
  modelId: string
}

function readEnv(key: string): string | undefined {
  const raw = (globalThis as {
    process?: { env?: Record<string, string | undefined> }
  }).process?.env?.[key]
  return typeof raw === 'string' ? raw : undefined
}

function isFalsy(value: string | undefined): boolean {
  return value !== undefined && FALSY.test(value.trim())
}

function isValidBinding(
  value: MediaSidecarConsentBinding | null | undefined,
): value is MediaSidecarConsentBinding {
  return (
    typeof value?.provider === 'string' &&
    value.provider.trim().length > 0 &&
    typeof value.modelId === 'string' &&
    value.modelId.trim().length > 0
  )
}

export function isMediaSidecarConsentEnabled(): boolean {
  const user = getSettingsForSource('userSettings')?.mediaSidecar ?? null
  const policy = getSettingsForSource('policySettings')?.mediaSidecar ?? null
  return (
    !isFalsy(readEnv(MEDIA_SIDECAR_ENV)) &&
    policy?.enabled !== false &&
    user?.enabled === true &&
    isValidBinding(user.consent)
  )
}

export function getMediaSidecarConsentBinding(): MediaSidecarConsentBinding | null {
  const value = getSettingsForSource('userSettings')?.mediaSidecar?.consent
  if (!isValidBinding(value)) return null
  return {
    provider: value.provider.trim(),
    modelId: value.modelId.trim(),
  }
}

export type MediaSidecarDiskCacheInputs = {
  cacheEnv?: string
  user?: { diskCacheEnabled?: boolean } | null
  policy?: { diskCacheEnabled?: boolean } | null
}

export function resolveMediaSidecarDiskCacheEnabled(
  input: MediaSidecarDiskCacheInputs,
): boolean {
  if (isFalsy(input.cacheEnv)) return false
  return (
    input.user?.diskCacheEnabled !== false &&
    input.policy?.diskCacheEnabled !== false
  )
}

export function isMediaSidecarDiskCacheEnabled(): boolean {
  return resolveMediaSidecarDiskCacheEnabled({
    cacheEnv: readEnv(MEDIA_SIDECAR_DISK_CACHE_ENV),
    user: getSettingsForSource('userSettings')?.mediaSidecar ?? null,
    policy: getSettingsForSource('policySettings')?.mediaSidecar ?? null,
  })
}
