import { readFileSync } from 'fs'
import { join } from 'path'
import { getCrabCodeConfigHomeDir } from '../../utils/envUtils.js'

interface ClientRegistration {
  client_id: string
  server_url?: string
}

export function loadClientRegistration(): ClientRegistration | null {
  try {
    // Canonical isolation-dir resolution (CONFIG_DIR > HOME > homedir);
    // previously re-implemented inline and DROPPED CRABCODE_HOME. Catch-all
    // try/catch below returns null, so this cannot throw on the hot path.
    const configDir = getCrabCodeConfigHomeDir()
    const raw = readFileSync(
      join(configDir, '.oauth-registration.json'),
      'utf-8',
    )
    const data = JSON.parse(raw)
    if (data?.client_id) {
      return data as ClientRegistration
    }
    return null
  } catch {
    return null
  }
}
