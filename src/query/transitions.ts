// Transition types for query state machine
export type Terminal = {
  reason: string
  error?: unknown
  turnCount?: number
  [key: string]: unknown
}

export type Continue = {
  reason?: string
  committed?: number
  messages?: unknown[]
  attempt?: number
  [key: string]: unknown
}
