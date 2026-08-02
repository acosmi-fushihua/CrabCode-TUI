const secretFieldNames = [
  'session_ingress_token',
  'environment_secret',
  'access_token',
  'refresh_token',
  'client_secret',
  'code_verifier',
  'code',
  'secret',
  'token',
]

const quotedSecretPattern = new RegExp(
  `"(${secretFieldNames.join('|')})"\\s*:\\s*"([^"]*)"`,
  'gi',
)

/** Redact known credential fields before writing diagnostic output. */
export function redactSecrets(value: string): string {
  return value.replace(quotedSecretPattern, '"$1":"[REDACTED]"')
}
