import { getOrganizationUUID } from '../oauth/client.js'
import { getAcosmiOAuthTokens } from '../../utils/auth.js'

/** Resolve the OAuth identity required by authenticated Acosmi API calls. */
export async function prepareOAuthApiRequest(): Promise<{
  accessToken: string
  orgUUID: string
}> {
  const accessToken = getAcosmiOAuthTokens()?.accessToken
  if (!accessToken) {
    throw new Error(
      'This action requires an Acosmi OAuth account. Run /login to authenticate.',
    )
  }

  const orgUUID = await getOrganizationUUID()
  if (!orgUUID) {
    throw new Error('Unable to get organization UUID')
  }

  return { accessToken, orgUUID }
}

/** Standard JSON headers for authenticated Acosmi API calls. */
export function getOAuthHeaders(accessToken: string): Record<string, string> {
  return {
    Authorization: `Bearer ${accessToken}`,
    'Content-Type': 'application/json',
  }
}
