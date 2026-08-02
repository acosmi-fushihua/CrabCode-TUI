/**
 * Types for the secure storage abstraction layer.
 *
 * SecureStorageData — a plain key/value record of stored credentials.
 * SecureStorage — the interface that all storage backends must satisfy.
 *
 * Every backend exposes async write siblings (`updateAsync` /
 * `deleteAsync`) plus a serialized read-modify-write primitive
 * (`mutateAsync`). The async methods are the runtime-standard entry points
 * while the sync `read` / `update` / `delete` survive only for the
 * startup / proven-cache-hit / pure-sync-API exemptions. Caller migration to
 * the async surface is T5 (P1-AUTH-CALLERS) — not done here.
 */

/** The shape of data stored in secure storage (OAuth tokens, API keys, etc.). */
export type SecureStorageData = Record<string, unknown>

/**
 * Result of a write/update operation.
 * `{ success: true }` on success, optionally with a non-fatal `warning`
 * (e.g. "Storing credentials in plaintext"); `{ success: false }` on failure.
 */
export type SecureStorageWriteResult = {
  success: boolean
  warning?: string
  /** True when the authoritative commit happened but a post-commit repair failed. */
  committed?: boolean
}

export type SecureStorageReadStatus =
  | { status: 'data'; data: SecureStorageData }
  | { status: 'absent' }
  | { status: 'error' }

/**
 * Read-modify-write callback for `mutateAsync`. Receives the freshest
 * committed record (credential keys only — storage-internal metadata such as
 * the generation stamp is already stripped) and returns the next full record
 * to persist. May be sync or async.
 */
export type SecureStorageMutator = (
  current: SecureStorageData,
) => SecureStorageData | Promise<SecureStorageData>

/** Interface implemented by all secure storage backends. */
export interface SecureStorage {
  /** Human-readable name for the backend (used in fallback naming). */
  readonly name: string

  /**
   * Synchronously read stored data.
   * Returns null if nothing is stored or the read fails.
   *
   * Sync read is reserved for the RFC §1.2 exemptions (startup / module init,
   * a provably-warm cache, or a pure-sync API with no event-loop sensitivity).
   * Runtime code paths must use `readAsync()`.
   */
  read(): SecureStorageData | null

  /**
   * Asynchronously read stored data — the runtime-standard read entry point.
   * Returns null if nothing is stored or the read fails.
   */
  readAsync(): Promise<SecureStorageData | null>

  /**
   * Optional authoritative read used by full-record read-modify-write owners.
   * Unlike `readAsync`, it distinguishes a definitely absent record from a
   * transient/corrupt read; callers must fail closed on `error` rather than
   * overwriting an inaccessible newer generation from a stale fallback.
   */
  readStatusAsync?(): Promise<SecureStorageReadStatus>

  /**
   * Synchronously write/update stored data (full-record overwrite).
   * Reserved for the RFC §1.2 sync exemptions; runtime code uses
   * `updateAsync()` / `mutateAsync()`.
   */
  update(data: SecureStorageData): SecureStorageWriteResult

  /**
   * Asynchronously write/update stored data (full-record overwrite) — async
   * sibling of `update()`.
   *
   * Use only when the caller owns 100% of the record. To change a subset of
   * keys use `mutateAsync()` instead, so a concurrent writer cannot clobber
   * the keys this caller did not touch.
   */
  updateAsync(data: SecureStorageData): Promise<SecureStorageWriteResult>

  /**
   * Serialized read-modify-write — the runtime-standard write entry point.
   *
   * `fn` runs inside a process-level FIFO critical section: the current
   * record is read, passed to `fn`, and the record `fn` returns is persisted,
   * with no other `mutateAsync` call interleaving between the read and the
   * write. This structurally eliminates the lost-update race that a bare
   * `read()` + `update()` pair is exposed to (RFC §2).
   */
  mutateAsync(fn: SecureStorageMutator): Promise<SecureStorageWriteResult>

  /**
   * Synchronously delete all stored data.
   * Returns true if deletion succeeded (including ENOENT / already-absent).
   * Reserved for the RFC §1.2 sync exemptions.
   */
  delete(): boolean

  /**
   * Asynchronously delete all stored data — the runtime-standard delete entry
   * point. Returns true if deletion succeeded (including ENOENT /
   * already-absent).
   */
  deleteAsync(): Promise<boolean>
}
