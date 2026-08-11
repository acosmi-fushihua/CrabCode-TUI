export const commandCatalogChangedSubtype =
  'crabcode_tui_command_catalog_changed'
export const commandCatalogProtocolVersion = 1

export function expectedTuiRuntimeIdentity(releaseMaterials) {
  if (
    !releaseMaterials ||
    typeof releaseMaterials.version !== 'string' ||
    typeof releaseMaterials.buildId !== 'string' ||
    releaseMaterials.version.length === 0 ||
    releaseMaterials.buildId.length === 0
  ) {
    throw new Error(
      'release materials must contain a non-empty version and buildId',
    )
  }
  return {
    version: releaseMaterials.version,
    buildId: releaseMaterials.buildId,
  }
}

export function assertTuiRuntimeSmokeSuccess(report) {
  if (
    !report ||
    typeof report !== 'object' ||
    Array.isArray(report) ||
    report.rendererContext !== 'received' ||
    report.initialize !== 'success' ||
    report.costTurns !== '2/2 success' ||
    report.endSession !== 'success' ||
    report.exitCode !== 0
  ) {
    throw new Error(
      `TUI runtime smoke report is not successful: ${JSON.stringify(report)}`,
    )
  }
  return report
}

function assertExactKeys(value, required, optional, label) {
  if (!value || typeof value !== 'object' || Array.isArray(value)) {
    throw new Error(`${label} must be an object: ${JSON.stringify(value)}`)
  }
  const allowed = new Set([...required, ...optional])
  const keys = Object.keys(value)
  if (
    required.some(key => !Object.hasOwn(value, key)) ||
    keys.some(key => !allowed.has(key))
  ) {
    throw new Error(
      `${label} has invalid fields: ${JSON.stringify(value)}`,
    )
  }
}

/**
 * Mirrors parseSlashCommand('/' + name) round-tripping to the same name with
 * no argument tail, without importing application source into package smoke.
 */
export function commandNameRoundTrips(name) {
  return (
    typeof name === 'string' &&
    name.length > 0 &&
    name.length <= 512 &&
    (!/\s/u.test(name) || /^[^\s]+ \(MCP\)$/u.test(name))
  )
}

export function assertCommandCatalogChangedRequest(frame) {
  if (typeof frame.request_id !== 'string' || frame.request_id.length === 0) {
    throw new Error(
      `command-catalog refresh has an invalid request_id: ${JSON.stringify(frame)}`,
    )
  }
  const request = frame.request
  assertExactKeys(
    request,
    ['subtype', 'protocol_version', 'commands'],
    [],
    'command-catalog refresh request',
  )
  if (
    request.subtype !== commandCatalogChangedSubtype ||
    request.protocol_version !== commandCatalogProtocolVersion ||
    !Array.isArray(request.commands) ||
    request.commands.length > 4_096
  ) {
    throw new Error(
      `command-catalog refresh request has invalid metadata: ${JSON.stringify(request)}`,
    )
  }

  const names = new Set()
  for (const [index, command] of request.commands.entries()) {
    const label = `command-catalog refresh commands[${index}]`
    assertExactKeys(
      command,
      ['name', 'description', 'argumentHint'],
      ['hidden', 'builtin'],
      label,
    )
    if (
      !commandNameRoundTrips(command.name) ||
      typeof command.description !== 'string' ||
      command.description.length > 16_384 ||
      typeof command.argumentHint !== 'string' ||
      command.argumentHint.length > 4_096 ||
      (Object.hasOwn(command, 'hidden') && command.hidden !== true) ||
      (Object.hasOwn(command, 'builtin') && command.builtin !== true) ||
      names.has(command.name)
    ) {
      throw new Error(`${label} is invalid: ${JSON.stringify(command)}`)
    }
    names.add(command.name)
  }
  return request.commands.length
}

export function commandCatalogChangedAck() {
  return {
    protocol_version: commandCatalogProtocolVersion,
    received: true,
  }
}
