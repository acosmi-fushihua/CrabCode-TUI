import axios from 'axios'
import { getOauthConfig } from '../../constants/oauth.js'
import { getOAuthHeaders, prepareOAuthApiRequest } from '../api/oauthRequest.js'
import { toError } from '../../utils/errors.js'
import { logError } from '../../utils/log.js'

export type EnvironmentKind = 'acosmi_cloud' | 'byoc'
export type EnvironmentState = 'active'

export type EnvironmentResource = {
  kind: EnvironmentKind
  environment_id: string
  name: string
  created_at: string
  state: EnvironmentState
}

export type EnvironmentListResponse = {
  environments: EnvironmentResource[]
  has_more: boolean
  first_id: string | null
  last_id: string | null
}

/** Fetch execution environments available to the TUI remote-agent tool. */
export async function fetchEnvironments(): Promise<EnvironmentResource[]> {
  const { accessToken, orgUUID } = await prepareOAuthApiRequest()
  const url = `${getOauthConfig().BASE_API_URL}/v1/environment_providers`

  try {
    const response = await axios.get<EnvironmentListResponse>(url, {
      headers: {
        ...getOAuthHeaders(accessToken),
        'x-organization-uuid': orgUUID,
      },
      timeout: 15000,
    })

    if (response.status !== 200) {
      throw new Error(
        `Failed to fetch environments: ${response.status} ${response.statusText}`,
      )
    }

    return response.data.environments
  } catch (error) {
    const err = toError(error)
    logError(err)
    throw new Error(`Failed to fetch environments: ${err.message}`)
  }
}

/** Create the default cloud environment used by scheduled remote agents. */
export async function createDefaultCloudEnvironment(
  name: string,
): Promise<EnvironmentResource> {
  const { accessToken, orgUUID } = await prepareOAuthApiRequest()
  const url = `${getOauthConfig().BASE_API_URL}/v1/environment_providers/cloud/create`
  const response = await axios.post<EnvironmentResource>(
    url,
    {
      name,
      kind: 'acosmi_cloud',
      description: '',
      config: {
        environment_type: 'acosmi',
        cwd: '/home/user',
        init_script: null,
        environment: {},
        languages: [
          { name: 'python', version: '3.11' },
          { name: 'node', version: '20' },
        ],
        network_config: {
          allowed_hosts: [],
          allow_default_hosts: true,
        },
      },
    },
    {
      headers: {
        ...getOAuthHeaders(accessToken),
        'x-organization-uuid': orgUUID,
      },
      timeout: 15000,
    },
  )
  return response.data
}
