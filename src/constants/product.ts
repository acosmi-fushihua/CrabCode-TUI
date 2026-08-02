export const CRAB_CODE_BRAND_NAME = 'Crab Code'
export const CRAB_CODE_ATTRIBUTION_EMAIL = 'noreply@acosmi.com'
export const CRAB_CODE_PRODUCT_URL = 'https://acosmi.com/crabcode'
export const CRAB_CODE_GITHUB_REPOSITORY_URL =
  'https://github.com/acosmi/crabcode'

export const PRODUCT_URL = CRAB_CODE_PRODUCT_URL
export const CRAB_CODE_COMMIT_TRAILER =
  `Co-authored-by: ${CRAB_CODE_BRAND_NAME} <${CRAB_CODE_ATTRIBUTION_EMAIL}>`
export const CRAB_CODE_PR_ATTRIBUTION =
  `Generated with [${CRAB_CODE_BRAND_NAME}](${CRAB_CODE_PRODUCT_URL})`

// CrabCode Remote session URLs
export const ACOSMI_BASE_URL = 'https://acosmi.com'
export const ACOSMI_STAGING_BASE_URL = 'https://staging.acosmi.com'
export const ACOSMI_LOCAL_BASE_URL = 'http://localhost:4000'

/**
 * Determine if we're in a staging environment for remote sessions.
 * Checks session ID format and ingress URL.
 */
export function isRemoteSessionStaging(
  sessionId?: string,
  ingressUrl?: string,
): boolean {
  return (
    sessionId?.includes('_staging_') === true ||
    ingressUrl?.includes('staging') === true
  )
}

/**
 * Determine if we're in a local-dev environment for remote sessions.
 * Checks session ID format (e.g. `session_local_...`) and ingress URL.
 */
export function isRemoteSessionLocal(
  sessionId?: string,
  ingressUrl?: string,
): boolean {
  return (
    sessionId?.includes('_local_') === true ||
    ingressUrl?.includes('localhost') === true
  )
}

/**
 * Get the base URL for CrabCode AI based on environment.
 */
export function getAcosmiBaseUrl(
  sessionId?: string,
  ingressUrl?: string,
): string {
  if (isRemoteSessionLocal(sessionId, ingressUrl)) {
    return ACOSMI_LOCAL_BASE_URL
  }
  if (isRemoteSessionStaging(sessionId, ingressUrl)) {
    return ACOSMI_STAGING_BASE_URL
  }
  return ACOSMI_BASE_URL
}

/**
 * Get the full session URL for a remote session.
 *
 * Session identifiers are treated as opaque values owned by the service.
 */
export function getRemoteSessionUrl(
  sessionId: string,
  ingressUrl?: string,
): string {
  const baseUrl = getAcosmiBaseUrl(sessionId, ingressUrl)
  return `${baseUrl}/code/${sessionId}`
}
