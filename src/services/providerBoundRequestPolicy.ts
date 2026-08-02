/**
 * Provider-bound media requests intentionally use one non-replaying network
 * attempt. Keep the request and cross-process lease budgets in one place so a
 * still-legal request can never be mistaken for an abandoned lease.
 */
export const PROVIDER_BOUND_CHAT_TIMEOUT_MS = 11 * 60 * 1_000

/** A heartbeat is advisory; the stale budget alone still covers one request. */
export const DESCRIPTION_LEASE_HEARTBEAT_MS = 30 * 1_000

/**
 * The owner may spend the full provider timeout in its single fetch. The extra
 * minute covers final local route checks and scheduling jitter even if a
 * heartbeat is delayed.
 */
export const DESCRIPTION_LEASE_STALE_MS =
  PROVIDER_BOUND_CHAT_TIMEOUT_MS + 60 * 1_000

/** Waiters fail closed after a bounded period and never evict a live owner. */
export const DESCRIPTION_LEASE_WAIT_TIMEOUT_MS =
  DESCRIPTION_LEASE_STALE_MS + 60 * 1_000

export const DESCRIPTION_LEASE_POLL_MS = 50
