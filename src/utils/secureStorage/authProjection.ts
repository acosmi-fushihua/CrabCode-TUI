import type { SecureStorageData } from './types.js'

/** Remove only Acosmi account/device credentials and preserve all siblings. */
export function stripAcosmiAccountCredentials(
  current: SecureStorageData,
): SecureStorageData {
  const {
    acosmiOauth: _oauth,
    trustedDeviceToken: _trustedDevice,
    ...preserved
  } = current
  return preserved
}
