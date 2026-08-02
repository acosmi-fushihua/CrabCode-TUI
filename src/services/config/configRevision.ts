// Durable optimistic-concurrency generation for `config/read` +
// `config/crabcode/write`.
//
// The generation is deliberately owned by the process-level control worker,
// but persisted below the selected CrabCode config directory so it survives a
// runtime restart. Every process targeting the same config
// directory acquires the same `proper-lockfile` mutex before reading or
// advancing it. This is the outer (total-order) lock; the existing
// settings.json / GlobalConfig locks remain the inner file-specific locks.
//
// The sidecar also stores a fingerprint of the two disk truth sources. A
// `config/read` after another process (or a TUI direct-settings path) changed a
// file advances the generation before returning the snapshot. A successful
// typed write always advances exactly once, including edits whose state is not
// projected through `Config` (for example MCP/plugin operations).

import { createHash, randomUUID } from "node:crypto";
import {
  closeSync,
  existsSync,
  fsyncSync,
  mkdirSync,
  openSync,
  readFileSync,
  renameSync,
  unlinkSync,
  writeFileSync,
} from "node:fs";
import { homedir } from "node:os";
import { dirname, join } from "node:path";

import { lock } from "../../utils/lockfile.js";

const STATE_SCHEMA = 1;
const STATE_FILE_NAME = ".config-generation-v1.json";
const LOCK_TARGET_NAME = ".config-generation-v1";
const LOCK_RETRIES = 12;

interface PersistedConfigRevision {
  schema: typeof STATE_SCHEMA;
  version: number;
  fingerprint: string;
}

export interface ConfigRevisionLease {
  /**
   * Reconcile disk truth with the persisted sidecar and return the current
   * generation. An out-of-band disk change advances the generation once.
   */
  refresh(): Promise<number>;

  /**
   * Commit one successful typed config mutation. Always advances exactly
   * once, then snapshots the post-write disk fingerprint.
   */
  advance(): Promise<number>;
}

function isEnvTruthy(value: string | undefined): boolean {
  if (!value) return false;
  return ["1", "true", "yes", "on"].includes(value.trim().toLowerCase());
}

function configDir(): string {
  const explicit = process.env.CRABCODE_CONFIG_DIR;
  if (explicit) return explicit.normalize("NFC");
  const homeBase = process.env.CRABCODE_HOME || homedir();
  return join(homeBase, ".crabcode").normalize("NFC");
}

function legacyConfigBase(): string {
  return (
    process.env.CRABCODE_CONFIG_DIR ||
    process.env.CRABCODE_HOME ||
    homedir()
  ).normalize("NFC");
}

function oauthConfigSuffix(): string {
  if (process.env.CRABCODE_CUSTOM_OAUTH_URL) return "-custom-oauth";
  if (process.env.USER_TYPE === "ant") {
    if (isEnvTruthy(process.env.USE_LOCAL_OAUTH)) return "-local-oauth";
    if (isEnvTruthy(process.env.USE_STAGING_OAUTH)) return "-staging-oauth";
  }
  return "";
}

function truthSourcePaths(): string[] {
  const dir = configDir();
  const base = legacyConfigBase();
  // Include both user-settings filenames because `--cowork` can be seeded in
  // bootstrap process state rather than an env var. Hashing an unused missing
  // path is harmless; hashing both prevents a mode switch from hiding a
  // generation change.
  return [
    join(dir, "settings.json"),
    join(dir, "cowork_settings.json"),
    join(dir, ".config.json"),
    join(base, `.crabcode${oauthConfigSuffix()}.json`),
  ].filter((value, index, all) => all.indexOf(value) === index);
}

function fingerprintDiskTruth(): string {
  const hash = createHash("sha256");
  for (const path of truthSourcePaths().sort()) {
    hash.update("path\0");
    hash.update(path);
    hash.update("\0");
    try {
      hash.update("present\0");
      hash.update(readFileSync(path));
    } catch (error) {
      const code = (error as { code?: unknown })?.code;
      if (code !== "ENOENT") throw error;
      hash.update("missing\0");
    }
    hash.update("\0");
  }

  // These env values participate in the projected effective config or select
  // its on-disk truth source. A restart with different values must not reuse a
  // generation that described the previous environment.
  const relevantEnv = {
    ACOSMI_DEFAULT_MODEL: process.env.ACOSMI_DEFAULT_MODEL ?? null,
    ACOSMI_SMALL_FAST_MODEL: process.env.ACOSMI_SMALL_FAST_MODEL ?? null,
    CRABCODE_CUSTOM_OAUTH_URL:
      process.env.CRABCODE_CUSTOM_OAUTH_URL ?? null,
    CRABCODE_USE_COWORK_PLUGINS:
      process.env.CRABCODE_USE_COWORK_PLUGINS ?? null,
    USER_TYPE: process.env.USER_TYPE ?? null,
    USE_LOCAL_OAUTH: process.env.USE_LOCAL_OAUTH ?? null,
    USE_STAGING_OAUTH: process.env.USE_STAGING_OAUTH ?? null,
  };
  hash.update("env\0");
  hash.update(JSON.stringify(relevantEnv));
  return hash.digest("hex");
}

function statePath(): string {
  return join(configDir(), STATE_FILE_NAME);
}

function lockTargetPath(): string {
  return join(configDir(), LOCK_TARGET_NAME);
}

function parseState(raw: string, path: string): PersistedConfigRevision {
  let value: unknown;
  try {
    value = JSON.parse(raw);
  } catch (error) {
    throw new Error(
      `config generation state is invalid JSON at ${path}: ${
        error instanceof Error ? error.message : String(error)
      }`,
    );
  }
  if (
    typeof value !== "object" ||
    value === null ||
    (value as Record<string, unknown>).schema !== STATE_SCHEMA ||
    !Number.isSafeInteger((value as Record<string, unknown>).version) ||
    ((value as Record<string, unknown>).version as number) < 0 ||
    typeof (value as Record<string, unknown>).fingerprint !== "string" ||
    !/^[0-9a-f]{64}$/.test(
      (value as Record<string, unknown>).fingerprint as string,
    )
  ) {
    throw new Error(`config generation state has invalid shape at ${path}`);
  }
  return value as PersistedConfigRevision;
}

function readState(): PersistedConfigRevision | null {
  const path = statePath();
  try {
    return parseState(readFileSync(path, "utf8"), path);
  } catch (error) {
    if ((error as { code?: unknown })?.code === "ENOENT") return null;
    // Never reset a corrupt counter to zero: that would let a stale client
    // accidentally pass CAS after a restart. Atomic writes make corruption
    // exceptional, so fail closed and surface the operator-visible error.
    throw error;
  }
}

function fsyncParentDirectory(path: string): void {
  // Windows does not permit opening a directory with Node's ordinary `r`
  // flags. Its rename is still atomic; POSIX gets the additional directory
  // fsync needed for rename durability across a sudden power loss.
  if (process.platform === "win32") return;
  let fd: number | null = null;
  try {
    fd = openSync(dirname(path), "r");
    fsyncSync(fd);
  } finally {
    if (fd !== null) closeSync(fd);
  }
}

function writeState(state: PersistedConfigRevision): void {
  const path = statePath();
  const tempPath = `${path}.${process.pid}.${randomUUID()}.tmp`;
  mkdirSync(dirname(path), { recursive: true });
  let fd: number | null = null;
  try {
    fd = openSync(tempPath, "wx", 0o600);
    writeFileSync(fd, `${JSON.stringify(state)}\n`, "utf8");
    fsyncSync(fd);
    closeSync(fd);
    fd = null;
    renameSync(tempPath, path);
    fsyncParentDirectory(path);
  } catch (error) {
    if (fd !== null) {
      try {
        closeSync(fd);
      } catch {
        // Preserve the original write error.
      }
    }
    if (existsSync(tempPath)) {
      try {
        unlinkSync(tempPath);
      } catch {
        // Best-effort cleanup; the unique temp is never read as state.
      }
    }
    throw error;
  }
}

function nextVersion(current: number): number {
  if (!Number.isSafeInteger(current) || current < 0) {
    throw new Error(`config generation is not a safe integer: ${current}`);
  }
  if (current >= Number.MAX_SAFE_INTEGER) {
    throw new Error("config generation exhausted Number.MAX_SAFE_INTEGER");
  }
  return Math.max(current + 1, Date.now());
}

function initialState(): PersistedConfigRevision {
  return {
    schema: STATE_SCHEMA,
    version: Date.now(),
    fingerprint: fingerprintDiskTruth(),
  };
}

// proper-lockfile intentionally rejects a second lock attempt for the same
// canonical path from the same process. Queue those attempts locally first;
// independent processes are still serialized by the filesystem lock below.
const processLockTails = new Map<string, Promise<void>>();

async function acquireProcessLock(key: string): Promise<() => void> {
  const previous = processLockTails.get(key) ?? Promise.resolve();
  let releaseCurrent!: () => void;
  const current = new Promise<void>((resolve) => {
    releaseCurrent = resolve;
  });
  const tail = previous.catch(() => undefined).then(() => current);
  processLockTails.set(key, tail);
  await previous.catch(() => undefined);
  return () => {
    releaseCurrent();
    if (processLockTails.get(key) === tail) {
      processLockTails.delete(key);
    }
  };
}

/**
 * Run one config read/write transaction under the shared cross-process total
 * lock. The callback must not escape the lease beyond its lifetime.
 */
export async function withConfigRevisionLock<T>(
  operation: (lease: ConfigRevisionLease) => Promise<T>,
): Promise<T> {
  const key = lockTargetPath();
  const releaseProcess = await acquireProcessLock(key);
  try {
    return await withCrossProcessConfigRevisionLock(operation);
  } finally {
    releaseProcess();
  }
}

async function withCrossProcessConfigRevisionLock<T>(
  operation: (lease: ConfigRevisionLease) => Promise<T>,
): Promise<T> {
  const dir = configDir();
  mkdirSync(dir, { recursive: true });
  let compromised: Error | null = null;
  const release = await lock(lockTargetPath(), {
    realpath: false,
    retries: {
      retries: LOCK_RETRIES,
      factor: 1.5,
      minTimeout: 10,
      maxTimeout: 150,
      randomize: true,
    },
    stale: 60_000,
    update: 10_000,
    onCompromised: (error) => {
      // Do not throw from proper-lockfile's timer callback (that would become
      // an unhandled exception). The lease checks this flag before any state
      // transition and before returning success.
      compromised = error;
    },
  });

  try {
    const assertLockOwnership = (): void => {
      if (compromised) {
        throw new Error(
          `config generation lock compromised: ${compromised.message}`,
        );
      }
    };

    let state = readState();
    const ensureState = (): PersistedConfigRevision => {
      if (state) return state;
      state = initialState();
      writeState(state);
      return state;
    };
    const lease: ConfigRevisionLease = {
      async refresh(): Promise<number> {
        assertLockOwnership();
        const current = ensureState();
        const fingerprint = fingerprintDiskTruth();
        if (current.fingerprint !== fingerprint) {
          state = {
            schema: STATE_SCHEMA,
            version: nextVersion(current.version),
            fingerprint,
          };
          writeState(state);
        }
        return state!.version;
      },
      async advance(): Promise<number> {
        assertLockOwnership();
        const current = ensureState();
        state = {
          schema: STATE_SCHEMA,
          version: nextVersion(current.version),
          fingerprint: fingerprintDiskTruth(),
        };
        writeState(state);
        return state.version;
      },
    };

    const result = await operation(lease);
    assertLockOwnership();
    return result;
  } finally {
    await release();
  }
}

export const __configRevisionPathsForTest = {
  statePath,
  lockTargetPath,
};
