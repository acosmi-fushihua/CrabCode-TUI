export type TeamLifecycleMember = {
  name: string
  isActive?: boolean
}

export type TeamCreateValidationError = {
  result: false
  message: string
  errorCode: 9
}

/**
 * Renderer-neutral TeamCreate input authority.
 *
 * Keep this separate from tool presentation so the native TUI can replace the
 * historical React renderer without changing the backend decision.
 */
export function teamCreateValidationError(
  teamName: string | undefined,
): TeamCreateValidationError | null {
  if (teamName && teamName.trim().length > 0) return null
  return {
    result: false,
    message: 'team_name is required for TeamCreate',
    errorCode: 9,
  }
}

/**
 * A leader may own only one active team. The error text is part of the
 * existing tool behavior and is intentionally independent of any TUI.
 */
export function assertTeamCreateAllowed(
  existingTeam: string | undefined,
): void {
  if (!existingTeam) return
  throw new Error(
    `Already leading team "${existingTeam}". A leader can only manage one team at a time. Use TeamDelete to end the current team before creating a new one.`,
  )
}

/**
 * TeamDelete blocks only on non-lead members whose active flag is not
 * explicitly false. Undefined retains the historical "active" meaning.
 */
export function activeNonLeadTeamMemberNames(
  members: readonly TeamLifecycleMember[],
  teamLeadName: string,
): string[] {
  return members
    .filter(
      member =>
        member.name !== teamLeadName && member.isActive !== false,
    )
    .map(member => member.name)
}

/**
 * The successful/no-team TeamDelete result clears only team runtime state and
 * the queued inbox. All unrelated AppState fields are preserved.
 */
export function clearDeletedTeamRuntimeState<
  State extends {
    teamContext?: unknown
    inbox: { messages: unknown[] }
  },
>(
  previous: State,
): Omit<State, 'teamContext' | 'inbox'> & {
  teamContext: undefined
  inbox: { messages: never[] }
} {
  return {
    ...previous,
    teamContext: undefined,
    inbox: {
      messages: [],
    },
  }
}
