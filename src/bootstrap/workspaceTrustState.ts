let interactive = false
let sessionTrustAccepted = false

export function getWorkspaceSessionInteractive(): boolean {
  return interactive
}

export function setWorkspaceSessionInteractive(value: boolean): void {
  interactive = value
}

export function getWorkspaceSessionTrustAccepted(): boolean {
  return sessionTrustAccepted
}

export function setWorkspaceSessionTrustAccepted(value: boolean): void {
  sessionTrustAccepted = value
}

export function resetWorkspaceTrustStateForTesting(): void {
  if (process.env.NODE_ENV !== 'test') {
    throw new Error(
      'resetWorkspaceTrustStateForTesting can only be called in tests',
    )
  }
  interactive = false
  sessionTrustAccepted = false
}
