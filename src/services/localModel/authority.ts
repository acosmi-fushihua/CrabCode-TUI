// Local-model authority handlers.
//
// The first production catalog pass is empty until model-license owner sign-off
// lands. PR-7C adds a local, test-seamed download/install foundation only: no
// remote manifest fetches, no arbitrary caller URLs, no runtime spawning, and
// no notifications.

import { createHash } from "node:crypto";
import { promises as fs, accessSync, constants as fsConstants } from "node:fs";
import { homedir, totalmem } from "node:os";
import {
  basename,
  dirname,
  isAbsolute,
  join,
  resolve,
  sep,
} from "node:path";
import { fileURLToPath } from "node:url";

import { WorkerError } from "../rpc/workerError.js";
import type {
  canUseLocalModels as CanUseLocalModels,
} from "../../utils/entitlements/localModels.js";
import type {
  getMembershipGateInput as GetMembershipGateInput,
} from "../../utils/auth.js";

const INVALID_PARAMS = -32602;
type MethodContext = unknown;

// W-CUSTOM-MODEL-PLUS-GATE (2026-07-04): gate floor =「Plus」(wire code
// BASIC) — see src/utils/entitlements/planTier.ts.
const LOCAL_MODEL_ENTITLEMENT_ERROR =
  "local models require a Plus (or higher) paid subscription";

interface LocalModelEntitlementFns {
  getMembershipGateInput: typeof GetMembershipGateInput;
  canUseLocalModels: typeof CanUseLocalModels;
}

let localModelEntitlementFnsOverride: LocalModelEntitlementFns | null = null;

export function __setLocalModelEntitlementFnsForTest(
  fns: LocalModelEntitlementFns | null,
): void {
  localModelEntitlementFnsOverride = fns;
}

async function loadLocalModelEntitlementFns(): Promise<LocalModelEntitlementFns> {
  if (localModelEntitlementFnsOverride) return localModelEntitlementFnsOverride;
  const [auth, entitlements] = await Promise.all([
    import("../../utils/auth.js"),
    import("../../utils/entitlements/localModels.js"),
  ]);
  return {
    getMembershipGateInput: auth.getMembershipGateInput,
    canUseLocalModels: entitlements.canUseLocalModels,
  };
}

// `ds4` = antirez DeepSeek V4 Flash Metal inference engine the user installs
// themselves (https://github.com/antirez/ds4) and points us at via
// `CRABCODE_DS4_SERVER`. The `ds4` / DeepSeek-V4 naming is a deliberate product
// decision allowed ONLY inside this local-model subsystem (catalog / runtime /
// guide copy + the Rust `LocalModelRuntime::Ds4` variant + `DS4_INSTALL_URL`).
// It must NEVER leak into the gateway ModelCapability wrappers under
// `src/utils/model/`; no model brands are hardcoded here.
export type LocalModelRuntime = "llama-server" | "ds4";
export type LocalModelProtocol = "openai-compatible";

/**
 * Declarative per-runtime engine differences. `server/start` reads the spec for
 * the requested runtime instead of branching on `useDs4` across binary
 * resolution / the auto-download gate / the missing-binary reason / the PATH
 * fallback. Adding an engine is one row here, not a hunt through those sites.
 *
 * Intentionally DATA-ONLY — no lifecycle methods. An engine with a
 * fundamentally different runtime model (e.g. a long-lived daemon with its own
 * pull/list/rm) brings its own behavior code; only its *declarative* knobs
 * (env var, whether we may auto-download, the not-found reason, the PATH
 * fallback name) extend this table. Keeping it data-only is what prevents a
 * future daemon engine from having to rework a premature lifecycle interface.
 *
 * The `Record<LocalModelRuntime, EngineSpec>` type makes completeness a
 * COMPILE-TIME invariant: adding a `LocalModelRuntime` variant without a spec
 * fails typecheck — no runtime "spec missing" crash is possible.
 */
interface EngineSpec {
  /** Env var holding an explicit binary path the user points us at. */
  readonly binaryEnvVar: string;
  /**
   * Whether `server/start` may auto-download the official pre-built engine when
   * neither an injected starter nor an env binary is present. ds4 is
   * user-installed only, so it never auto-downloads.
   */
  readonly autoDownload: boolean;
  /** `server/status.reason` when no binary for this runtime can be resolved. */
  readonly missingBinaryReason: string;
  /** Bare binary name used as the last-resort PATH fallback. */
  readonly fallbackBinaryName: string;
}

const ENGINE_SPECS: Record<LocalModelRuntime, EngineSpec> = {
  "llama-server": {
    binaryEnvVar: "CRABCODE_LLAMA_SERVER",
    autoDownload: true,
    missingBinaryReason: "missing-llama-server",
    fallbackBinaryName: "llama-server",
  },
  ds4: {
    binaryEnvVar: "CRABCODE_DS4_SERVER",
    autoDownload: false,
    missingBinaryReason: "missing-ds4-server",
    fallbackBinaryName: "ds4",
  },
};

/** Type guard: is `value` a known local engine runtime (has an EngineSpec)? */
function isKnownRuntime(value: string): value is LocalModelRuntime {
  return Object.prototype.hasOwnProperty.call(ENGINE_SPECS, value);
}

/**
 * Install/guide URL for the user-provided ds4 engine. Surfaced to the local-model
 * guide UI only; never referenced from the gateway model wrappers.
 */
export const DS4_INSTALL_URL = "https://github.com/antirez/ds4";

/**
 * Minimum unified memory ds4 (DeepSeek V4 Flash) needs on Apple Silicon before
 * we mark it as a ready flagship option (128 GiB).
 */
const DS4_MIN_UNIFIED_MEMORY_BYTES = 128 * 1024 * 1024 * 1024;
export type LocalModelFormat = "gguf";
// BYO-local adds the `user-local-path` source alongside the curated origin.
export type LocalModelSource = "curated" | "user-local-path";
export type LocalModelCatalogStatus = "awaiting-license-signoff";
export type LocalModelInstallStatus = "not-installed" | "installed";
export type LocalModelDownloadStatus =
  | "queued"
  | "downloading"
  | "completed"
  | "failed"
  | "cancelled"
  | "not-found"
  | "unavailable";

export interface WorkerLocalModelCatalogEntry {
  id: string;
  displayName: string;
  description: string;
  runtime: LocalModelRuntime;
  protocol: LocalModelProtocol;
  format: LocalModelFormat;
  source: LocalModelSource;
  license: string;
  sizeBytes: number;
  sha256: string;
  installed: boolean;
  status: LocalModelInstallStatus;
}

export interface WorkerLocalModelCuratedEntry extends WorkerLocalModelCatalogEntry {
  sourceUri: string;
  fileName?: string;
}

/**
 * BYO-local (Bring-Your-Own GGUF) registry record.
 *
 * The user points at an existing `.gguf` file anywhere on the local machine;
 * we *reference it in place* (never copy — GGUF weights can be tens of GB) and
 * persist this record to `{root}/byo/registry.json`. `ggufPath` is therefore an
 * arbitrary absolute host path outside the local-models root; only the registry
 * file itself is constrained to live inside the root.
 */
export interface WorkerLocalModelByoRecord {
  id: string;
  displayName: string;
  /** Absolute path to the user's `.gguf` file (referenced, not copied). */
  ggufPath: string;
  sizeBytes: number;
  addedAtMs: number;
}

export interface WorkerLocalModelCatalogReadResult {
  data: WorkerLocalModelMergedCatalogEntry[];
  source: LocalModelSource;
  manifestStatus: LocalModelCatalogStatus;
  manifestVersion: number;
}

/**
 * Wire shape of a catalog entry as returned to the Rust facade. Conforms to the
 * ts-rs-generated `LocalModelCatalogEntry` (richer than the internal
 * `WorkerLocalModelCatalogEntry`): `description` / `license` / `sizeBytes` /
 * `sha256` / `modelPath` / `reason` are nullable, `status` spans the full
 * `LocalModelCatalogStatus` enum, and `source` includes `user-local-path`.
 */
export interface WorkerLocalModelMergedCatalogEntry {
  id: string;
  displayName: string;
  description: string | null;
  runtime: LocalModelRuntime;
  protocol: LocalModelProtocol;
  format: LocalModelFormat;
  source: LocalModelSource;
  license: string | null;
  sizeBytes: number | null;
  sha256: string | null;
  installed: boolean;
  status: string;
  modelPath: string | null;
  reason: string | null;
}

export interface WorkerLocalModelRuntimeSupport {
  id: LocalModelRuntime;
  displayName: string;
  supported: boolean;
  acceleration: "metal" | null;
  reason: string;
}

export interface WorkerLocalModelSystemProfileResult {
  platform: NodeJS.Platform;
  arch: string;
  memoryBytes: number | null;
  recommendedRuntime: LocalModelRuntime | null;
  supportedRuntimes: WorkerLocalModelRuntimeSupport[];
}

export interface WorkerLocalModelDownloadStatus {
  state: LocalModelDownloadStatus;
  reason: string | null;
  downloadId: string | null;
  modelId: string | null;
  bytesReceived: number | null;
  totalBytes: number | null;
  percentage: number | null;
  error: string | null;
}

export interface WorkerLocalModelDownloadResult {
  status: WorkerLocalModelDownloadStatus;
}

export interface WorkerLocalModelInstallRemoveResult {
  state: "removed" | "not-found" | "failed" | "unavailable";
  reason: string | null;
  modelId: string | null;
  modelPath: string | null;
}

// PR-7D: server lifecycle types
export type LocalModelServerState =
  | "stopped"
  | "starting"
  | "running"
  | "stopping"
  | "failed"
  | "unavailable";

export interface WorkerLocalModelServerStatus {
  state: LocalModelServerState;
  reason?: string;
  host?: string;
  port?: number;
  url?: string;
  pid?: number;
  modelId?: string;
  modelPath?: string;
  error?: string;
  stderrTail?: string;
}

export interface WorkerLocalModelServerResult {
  status: WorkerLocalModelServerStatus;
}

export interface LocalModelServerStarterArgs {
  binary: string;
  argv: string[];
  modelPath: string;
  host: string;
  port: number;
}

/**
 * A spawned local-model server process handle (BLOCKER-3, merged audit
 * 2026-05-21). Production wraps `Bun.Subprocess`; tests inject a fake.
 *
 * Holding the handle — not just the pid — is what makes crash monitoring
 * (`exited`) and precise termination (`kill`) reliable, even within a single
 * worker lifetime. The prior implementation discarded the `Bun.Subprocess`
 * and kept only the pid, so it could neither detect a crash nor kill the
 * exact child.
 */
export interface LocalModelServerProcess {
  pid: number;
  /** Resolves with the exit code (or null on signal) once the process ends. */
  exited: Promise<number | null>;
  /** Send a termination signal to the process. */
  kill(signal?: NodeJS.Signals | number): void;
  /** Best-effort recent stderr text, for diagnosing a failed start. */
  readStderrTail?: () => string;
}

export type LocalModelServerStarter = (
  args: LocalModelServerStarterArgs,
) => Promise<LocalModelServerProcess>;

/** Probe a server URL for readiness; resolve true when `/health` answers ok. */
export type LocalModelHealthProbe = (
  url: string,
  signal: AbortSignal,
) => Promise<boolean>;

/** Reserve a free loopback TCP port. */
export type LocalModelPortReserver = () => Promise<number>;

/** Tunable lifecycle timings; overridable in tests for fast deterministic runs. */
export interface LocalModelServerTimings {
  /** Budget for the post-spawn `/health` readiness wait inside server/start. */
  startHealthWaitMs: number;
  /** Poll interval while waiting for `/health`. */
  healthPollIntervalMs: number;
  /** Grace period after SIGTERM before escalating to SIGKILL. */
  stopGraceMs: number;
  /** Timeout for a single `/health` probe request. */
  healthProbeTimeoutMs: number;
}

interface InternalDownloadState extends WorkerLocalModelDownloadResult {
  artifactPath: string | null;
  sha256: string | null;
}

interface SystemProfileProbe {
  platform: NodeJS.Platform;
  arch: string;
  memoryBytes: number | null;
}

interface InstalledLocalModelMetadata {
  modelId: string;
  artifactPath: string;
  sha256: string;
  sizeBytes: number;
  installedAt: string;
}

let systemProfileProbeOverride: (() => SystemProfileProbe) | null = null;
let curatedCatalogOverride: WorkerLocalModelCuratedEntry[] | null = null;
const downloads = new Map<string, InternalDownloadState>();

/**
 * Test-only seam for the BYO `.gguf` stat/access probe. Production hits the
 * real filesystem; tests inject a deterministic result so they never depend on
 * a real multi-GB file existing on disk.
 */
interface ByoGgufProbe {
  exists: boolean;
  isFile: boolean;
  readable: boolean;
  sizeBytes: number;
}
let byoGgufProbeOverride: ((path: string) => Promise<ByoGgufProbe>) | null = null;

/**
 * Test-only seam for the ds4 binary install probe. Production resolves the real
 * binary from `CRABCODE_DS4_SERVER` (or a `ds4` / `ds4-server` on PATH); tests
 * inject a deterministic boolean so they never depend on a real ds4 install.
 */
let ds4BinaryProbeOverride: (() => boolean) | null = null;

// PR-7D + BLOCKER-3: one in-memory running server state.
interface CurrentServerState {
  state: LocalModelServerState;
  reason?: string;
  host: string;
  port: number;
  url: string;
  pid?: number;
  modelId?: string;
  modelPath: string;
  error?: string;
  stderrTail?: string;
  /** Live process handle, when one was spawned (absent in failed states). */
  process?: LocalModelServerProcess;
  /** Set once `process.exited` resolves — distinguishes a crash from running. */
  exitCode?: number | null;
  /** Monotonic id distinguishing successive start attempts; lets a stale
   *  `exited` callback know whether it still owns `currentServer`. */
  generation: number;
}

const LOOPBACK_HOST = "127.0.0.1";

const DEFAULT_SERVER_TIMINGS: LocalModelServerTimings = {
  startHealthWaitMs: 8000,
  healthPollIntervalMs: 250,
  stopGraceMs: 5000,
  healthProbeTimeoutMs: 2000,
};

let currentServer: CurrentServerState | null = null;
let serverGeneration = 0;
let workerRuntimeShuttingDown = false;
let serverStarterOverride: LocalModelServerStarter | null = null;
let healthProbeOverride: LocalModelHealthProbe | null = null;
let portReserverOverride: LocalModelPortReserver | null = null;
let serverTimingsOverride: Partial<LocalModelServerTimings> | null = null;

/**
 * Test-only seam for deterministic platform / arch / memory assertions.
 * Production code must use the real process + os probes below.
 */
export function __setLocalModelSystemProfileProbeForTest(
  probe: (() => SystemProfileProbe) | null,
): void {
  systemProfileProbeOverride = probe;
}

/**
 * Test-only seam for the ds4 binary install probe. Production hits the real
 * `CRABCODE_DS4_SERVER` env / PATH lookup via `detectDs4BinaryInstalled()`.
 */
export function __setLocalModelDs4ProbeForTest(
  probe: (() => boolean) | null,
): void {
  ds4BinaryProbeOverride = probe;
}

/**
 * Test-only seam for PR-7C. Production keeps the catalog empty until the
 * model-license owner signs curated entries.
 */
export function __setLocalModelCatalogEntriesForTest(
  entries: WorkerLocalModelCuratedEntry[] | null,
): void {
  curatedCatalogOverride = entries;
}

/**
 * Test-only seam for PR-7D / BLOCKER-3. Inject a fake process starter so tests
 * can verify argv and lifecycle without spawning a real llama-server. The fake
 * process's own `kill` / `exited` drive stop and crash handling.
 */
export function __setLocalModelServerStarterForTest(
  starter: LocalModelServerStarter | null,
): void {
  serverStarterOverride = starter;
}

/** Test-only seam: inject a deterministic `/health` probe. */
export function __setLocalModelHealthProbeForTest(
  probe: LocalModelHealthProbe | null,
): void {
  healthProbeOverride = probe;
}

/** Test-only seam: inject a deterministic free-port reservation. */
export function __setLocalModelPortReserverForTest(
  reserver: LocalModelPortReserver | null,
): void {
  portReserverOverride = reserver;
}

/** Test-only seam: shrink lifecycle timings so tests run fast. */
export function __setLocalModelServerTimingsForTest(
  timings: Partial<LocalModelServerTimings> | null,
): void {
  serverTimingsOverride = timings;
}

/**
 * Test-only seam: inject a deterministic BYO `.gguf` stat/access probe so
 * `byo/add` validation can be exercised without a real file on disk.
 */
export function __setLocalModelByoGgufProbeForTest(
  probe: ((path: string) => Promise<ByoGgufProbe>) | null,
): void {
  byoGgufProbeOverride = probe;
}

export function __resetLocalModelStateForTest(): void {
  systemProfileProbeOverride = null;
  curatedCatalogOverride = null;
  localModelEntitlementFnsOverride = null;
  downloads.clear();
  currentServer = null;
  serverGeneration = 0;
  workerRuntimeShuttingDown = false;
  serverStarterOverride = null;
  healthProbeOverride = null;
  portReserverOverride = null;
  serverTimingsOverride = null;
  byoGgufProbeOverride = null;
  engineDownloaderOverride = null;
  engineState = { state: "unknown" };
  engineEnsureInflight = null;
}

function validateEmptyObjectParams(raw: unknown): void {
  if (raw === undefined || raw === null) return;
  if (typeof raw !== "object" || raw === null || Array.isArray(raw)) {
    throw new WorkerError(INVALID_PARAMS, "params must be an object");
  }
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function requireModelId(raw: unknown): string {
  if (!isRecord(raw)) {
    throw new WorkerError(INVALID_PARAMS, "params must be an object");
  }
  const modelId = raw.modelId;
  if (typeof modelId !== "string" || modelId.length === 0) {
    throw new WorkerError(
      INVALID_PARAMS,
      "params.modelId must be a non-empty string",
    );
  }
  validateModelId(modelId);
  return modelId;
}

function readDownloadLocator(raw: unknown): { downloadId?: string; modelId?: string } {
  if (!isRecord(raw)) {
    throw new WorkerError(INVALID_PARAMS, "params must be an object");
  }
  const downloadId = raw.downloadId;
  const modelId = raw.modelId;
  const out: { downloadId?: string; modelId?: string } = {};
  if (downloadId !== undefined && downloadId !== null) {
    if (typeof downloadId !== "string" || downloadId.length === 0) {
      throw new WorkerError(
        INVALID_PARAMS,
        "params.downloadId must be a non-empty string",
      );
    }
    out.downloadId = downloadId;
  }
  if (modelId !== undefined && modelId !== null) {
    if (typeof modelId !== "string" || modelId.length === 0) {
      throw new WorkerError(
        INVALID_PARAMS,
        "params.modelId must be a non-empty string",
      );
    }
    validateModelId(modelId);
    out.modelId = modelId;
  }
  if (!out.downloadId && !out.modelId) {
    throw new WorkerError(
      INVALID_PARAMS,
      "params.downloadId or params.modelId is required",
    );
  }
  return out;
}

function validateModelId(modelId: string): void {
  if (
    !/^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$/.test(modelId) ||
    modelId.includes("..") ||
    modelId.includes("/") ||
    modelId.includes("\\")
  ) {
    throw new WorkerError(
      INVALID_PARAMS,
      "params.modelId must be a safe curated model id",
    );
  }
}

function readSystemProfile(): SystemProfileProbe {
  if (systemProfileProbeOverride) return systemProfileProbeOverride();
  return {
    platform: process.platform,
    arch: process.arch,
    memoryBytes: totalmem(),
  };
}

/**
 * Detect whether the user-provided ds4 engine is installed. Production checks
 * `CRABCODE_DS4_SERVER` (an explicit binary path the user points us at) and
 * falls back to a `ds4` / `ds4-server` on PATH. Test code overrides this through
 * `__setLocalModelDs4ProbeForTest` so it never depends on a real install.
 */
function detectDs4BinaryInstalled(): boolean {
  if (ds4BinaryProbeOverride) return ds4BinaryProbeOverride();

  const fromEnv = process.env.CRABCODE_DS4_SERVER;
  if (fromEnv && fromEnv.trim().length > 0) return true;

  // PATH lookup for a `ds4` / `ds4-server` binary the user installed globally.
  const pathEnv = process.env.PATH;
  if (!pathEnv) return false;
  const dirs = pathEnv.split(":").filter((d) => d.length > 0);
  for (const dir of dirs) {
    for (const name of ["ds4", "ds4-server"]) {
      try {
        const candidate = join(dir, name);
        // Synchronous existence + executable check; the catalog probe path is
        // cheap and infrequent (called only from systemProfile/read).
        accessSync(candidate, fsConstants.X_OK);
        return true;
      } catch {
        // not here; keep scanning
      }
    }
  }
  return false;
}

function resolveRuntimeSupport(
  profile: Pick<SystemProfileProbe, "platform" | "arch" | "memoryBytes">,
): Pick<WorkerLocalModelSystemProfileResult, "recommendedRuntime" | "supportedRuntimes"> {
  if (profile.platform === "darwin" && profile.arch === "arm64") {
    const supportedRuntimes: WorkerLocalModelRuntimeSupport[] = [
      {
        id: "llama-server",
        displayName: "llama-server",
        supported: true,
        acceleration: "metal",
        reason: "darwin-arm64-metal-baseline",
      },
    ];

    // ds4 is an optional flagship (NOT the default recommendation — llama-server
    // is more broadly applicable). It surfaces as a three-state capability the
    // guide UI can render: ready / needs-128GB / not-installed.
    const memoryBytes = profile.memoryBytes ?? 0;
    const meets128Gb = memoryBytes >= DS4_MIN_UNIFIED_MEMORY_BYTES;
    const ds4Installed = detectDs4BinaryInstalled();
    let ds4Supported: boolean;
    let ds4Reason: string;
    if (!meets128Gb) {
      ds4Supported = false;
      ds4Reason = "ds4-needs-128gb-unified-memory";
    } else if (!ds4Installed) {
      ds4Supported = false;
      ds4Reason = "ds4-binary-not-installed";
    } else {
      ds4Supported = true;
      ds4Reason = "darwin-arm64-128gb-ds4-ready";
    }
    supportedRuntimes.push({
      id: "ds4",
      displayName: "ds4 (DeepSeek V4 Flash · Metal)",
      supported: ds4Supported,
      acceleration: "metal",
      reason: ds4Reason,
    });

    return {
      // llama-server stays the recommended default; ds4 is visible as an
      // optional flagship in supportedRuntimes when it is ready.
      recommendedRuntime: "llama-server",
      supportedRuntimes,
    };
  }

  return {
    recommendedRuntime: null,
    supportedRuntimes: [],
  };
}

function resolveCrabCodeHome(): string {
  const fromEnv = process.env.CRABCODE_HOME;
  if (fromEnv && fromEnv.length > 0) return resolve(fromEnv);
  return join(homedir(), ".crabcode");
}

function resolveLocalModelsRoot(): string {
  return join(resolveCrabCodeHome(), "local-models");
}

function ensureInsideRoot(root: string, candidate: string): string {
  const resolvedRoot = resolve(root);
  const resolvedCandidate = resolve(candidate);
  if (
    resolvedCandidate !== resolvedRoot &&
    !resolvedCandidate.startsWith(resolvedRoot + sep)
  ) {
    throw new WorkerError(
      INVALID_PARAMS,
      "local model path escaped local-models root",
    );
  }
  return resolvedCandidate;
}

function metadataPath(root: string, modelId: string): string {
  return ensureInsideRoot(root, join(root, "metadata", `${modelId}.json`));
}

/** Path to the BYO registry JSON, always constrained inside the root. */
function byoRegistryPath(root: string): string {
  return ensureInsideRoot(root, join(root, "byo", "registry.json"));
}

function modelDir(root: string, modelId: string): string {
  return ensureInsideRoot(root, join(root, "models", modelId));
}

function tempPath(root: string, downloadId: string): string {
  return ensureInsideRoot(root, join(root, "tmp", `${downloadId}.tmp`));
}

// ---- On-demand llama.cpp inference engine download (W-MODEL-GATING) ----
//
// When a Pro user brings their own GGUF (BYO) but has not installed a
// `llama-server` binary (no `CRABCODE_LLAMA_SERVER` env, none on PATH), we
// fetch the matching platform pre-built `llama-server` from the official
// llama.cpp GitHub release (MIT). The archive is sha256-verified against a
// pinned digest, extracted into `{root}/engine/<tag>/`, and the resolved
// binary is reused on subsequent starts. This is an application-layer on-demand
// fetch (the engine is large, lifecycle-decoupled from the release tarball);
// it is NOT the §7 vendor contract (ripgrep ships in-tarball) — no ghproxy
// sniffing, no bootstrap.rs / release.yml vendor entry.
//
// `llama-server` is an engine binary name (consistent with the existing
// `CRABCODE_LLAMA_SERVER` env), not a model brand — §硬约束 #1 unaffected.

/** Pinned llama.cpp release tag the bundled engine assets come from. */
const LLAMA_CPP_ENGINE_TAG = "b9437";

/**
 * Per-platform llama.cpp release asset + its pinned sha256 (GitHub release
 * `digest` field). Only the four platforms with official pre-builds are listed;
 * any other `${platform}-${arch}` has no auto-download (callers fall back to
 * the existing `missing-llama-server` env path).
 */
const LLAMA_CPP_ENGINE_ASSETS: Record<string, { asset: string; sha256: string }> = {
  "darwin-arm64": {
    asset: "llama-b9437-bin-macos-arm64.tar.gz",
    sha256: "be62e359c081e718397e4ac9f8b7b346b77133681aa052bc6a26f5525ad0f723",
  },
  "darwin-x64": {
    asset: "llama-b9437-bin-macos-x64.tar.gz",
    sha256: "2a355c6c22fab70a47f25bff49b73083e0d59cb266a5cc2df5544bfd0b86e13d",
  },
  "linux-x64": {
    asset: "llama-b9437-bin-ubuntu-x64.tar.gz",
    sha256: "07b0bf370a696329463d999ecb5c4860717eef6824e55eaf062214d70e78174d",
  },
  "win32-x64": {
    asset: "llama-b9437-bin-win-cpu-x64.zip",
    sha256: "7f19b3da00425946e41a83c15f8ef4bf5cd261f35f941e408e9b2634ce8b6d7f",
  },
};

function engineRoot(root: string): string {
  return ensureInsideRoot(root, join(root, "engine"));
}

function engineVersionDir(root: string, tag: string): string {
  return ensureInsideRoot(root, join(root, "engine", tag));
}

/** llama-server binary name for the current platform. */
function llamaServerBinaryName(): string {
  return process.platform === "win32" ? "llama-server.exe" : "llama-server";
}

/**
 * Spec passed to the (test-seamable) downloader: it must fetch + sha256-verify
 * + extract the asset and return the absolute path to a runnable `llama-server`
 * inside `versionDir`. Production downloads from the official GitHub release.
 */
interface LlamaEngineDownloadSpec {
  url: string;
  asset: string;
  sha256: string;
  /** Temp file path (inside root) the archive is downloaded to. */
  tmpPath: string;
  /** `{root}/engine/<tag>/` — extraction target, constrained inside root. */
  versionDir: string;
  /** Binary basename to locate after extraction. */
  binaryName: string;
}

let engineDownloaderOverride:
  | ((spec: LlamaEngineDownloadSpec) => Promise<string>)
  | null = null;

/**
 * Test-only seam for the on-demand llama-server engine downloader. Production
 * performs the real fetch / sha256 verify / extract; tests inject a mock that
 * returns a fake binary path so they never hit the network or unpack archives.
 */
export function __setLocalModelEngineDownloaderForTest(
  downloader:
    | ((spec: LlamaEngineDownloadSpec) => Promise<string>)
    | null,
): void {
  engineDownloaderOverride = downloader;
}

// ---- Engine-ensure state machine ----
//
// The on-demand engine download used to run synchronously inside `server/start`
// — bounded by the 15s control-worker timeout, with zero progress UI. This
// state machine surfaces it as an explicit, pollable resource:
//   * `localModel/engine/ensure` kicks a NON-BLOCKING single-flight background
//     download and returns the current snapshot immediately.
//   * `localModel/engine/status` reads the snapshot (byte progress) for polling.
// `server/start` now also routes through `startEngineEnsure` (single-flight), so
// it can never double-download against a concurrent `ensure`.

type EngineEnsureStateInternal = {
  state:
    | "unknown"
    | "ready"
    | "downloading"
    | "extracting"
    | "unavailable"
    | "failed";
  receivedBytes?: number;
  totalBytes?: number;
  binaryPath?: string;
  reason?: string;
  error?: string;
};

/** Wire shape returned to the dispatcher (mirrors `LocalModelEngineStatus`). */
export interface WorkerLocalModelEngineStatus {
  state: EngineEnsureStateInternal["state"];
  tag: string;
  receivedBytes?: number;
  totalBytes?: number;
  binaryPath?: string;
  reason?: string;
  error?: string;
}

export interface WorkerLocalModelEngineResult {
  status: WorkerLocalModelEngineStatus;
}

let engineState: EngineEnsureStateInternal = { state: "unknown" };
let engineEnsureInflight: Promise<string | null> | null = null;

/**
 * Recursively locate an executable `binaryName` under `dir`. mac/linux release
 * archives place `llama-server` in `build/bin/` alongside its sibling
 * `libllama.*` / `libggml*.*` shared libraries, so we search the whole tree and
 * return the first match (the binary is run from its own directory, keeping the
 * sibling libs resolvable).
 */
async function findExecutableUnder(
  dir: string,
  binaryName: string,
): Promise<string | null> {
  let entries: import("node:fs").Dirent[];
  try {
    entries = await fs.readdir(dir, { withFileTypes: true });
  } catch {
    return null;
  }
  for (const entry of entries) {
    const full = join(dir, entry.name);
    if (entry.isDirectory()) {
      const nested = await findExecutableUnder(full, binaryName);
      if (nested) return nested;
    } else if (entry.isFile() && entry.name === binaryName) {
      return full;
    }
  }
  return null;
}

/**
 * Production downloader: fetch the asset, sha256-verify against the pinned
 * digest, extract into `versionDir`, chmod the binary, and return its path.
 * Throws a structured `WorkerError` on checksum mismatch.
 */
async function defaultEngineDownload(
  spec: LlamaEngineDownloadSpec,
  onProgress?: (receivedBytes: number, totalBytes: number | null) => void,
): Promise<string> {
  const res = await fetch(spec.url);
  if (!res.ok) {
    throw new WorkerError(
      INVALID_PARAMS,
      `engine-download-http-${res.status}`,
    );
  }
  // Stream the archive so the engine-ensure state machine can surface byte
  // progress (the asset is tens of MB). `Content-Length` is advisory (may be
  // absent / gzip-encoded); the sha256 check below remains the integrity gate.
  const totalHeader = res.headers.get("content-length");
  const total =
    totalHeader != null && /^\d+$/.test(totalHeader.trim())
      ? Number(totalHeader.trim())
      : null;
  let payload: Buffer;
  if (res.body && typeof res.body.getReader === "function") {
    const reader = res.body.getReader();
    const chunks: Uint8Array[] = [];
    let received = 0;
    onProgress?.(0, total);
    for (;;) {
      const { done, value } = await reader.read();
      if (done) break;
      if (value && value.length > 0) {
        chunks.push(value);
        received += value.length;
        onProgress?.(received, total);
      }
    }
    payload = Buffer.concat(chunks);
  } else {
    payload = Buffer.from(await res.arrayBuffer());
    onProgress?.(payload.length, total ?? payload.length);
  }
  const digest = sha256(payload);
  if (digest !== spec.sha256) {
    throw new WorkerError(INVALID_PARAMS, "engine-download-checksum-mismatch");
  }

  await fs.mkdir(dirname(spec.tmpPath), { recursive: true, mode: 0o700 });
  await fs.mkdir(spec.versionDir, { recursive: true, mode: 0o700 });
  await fs.writeFile(spec.tmpPath, payload, { mode: 0o600 });

  try {
    if (spec.asset.endsWith(".zip")) {
      // Windows: prefer system `tar` (ships with modern Windows) which unpacks
      // zip; `unzip` is the POSIX fallback.
      const unpacked = await runExtract(
        ["tar", "-xf", spec.tmpPath, "-C", spec.versionDir],
        spec.versionDir,
      );
      if (!unpacked) {
        await runExtract(
          ["unzip", "-o", spec.tmpPath, "-d", spec.versionDir],
          spec.versionDir,
          true,
        );
      }
    } else {
      await runExtract(
        ["tar", "-xzf", spec.tmpPath, "-C", spec.versionDir],
        spec.versionDir,
        true,
      );
    }
  } finally {
    await fs.rm(spec.tmpPath, { force: true });
  }

  const binaryPath = await findExecutableUnder(spec.versionDir, spec.binaryName);
  if (!binaryPath) {
    throw new WorkerError(
      INVALID_PARAMS,
      "engine-download-binary-not-found-after-extract",
    );
  }
  try {
    await fs.chmod(binaryPath, 0o755);
  } catch {
    /* best-effort; some platforms (win32) ignore chmod */
  }
  return binaryPath;
}

/** Run a system extract command via `Bun.spawn`; resolve true on exit 0. */
async function runExtract(
  cmd: string[],
  _cwd: string,
  throwOnFail = false,
): Promise<boolean> {
  let proc: import("bun").Subprocess;
  try {
    proc = Bun.spawn(cmd, { stdout: "ignore", stderr: "pipe" });
  } catch (error) {
    if (throwOnFail) {
      throw new WorkerError(
        INVALID_PARAMS,
        `engine-extract-spawn-failed: ${
          error instanceof Error ? error.message : String(error)
        }`,
      );
    }
    return false;
  }
  const code = await proc.exited;
  if (code !== 0) {
    if (throwOnFail) {
      throw new WorkerError(INVALID_PARAMS, `engine-extract-failed-code-${code}`);
    }
    return false;
  }
  return true;
}

/**
 * Resolve a usable `llama-server` for the current platform: reuse the cached
 * download if present, otherwise download + verify + extract on demand. Returns
 * the binary path, or `null` when the platform has no official pre-built asset
 * (caller then falls back to the existing `missing-llama-server` env path).
 *
 * Throws (caught by the caller as `engine-download-failed`) on download /
 * checksum / extract failure.
 */
async function resolveOrDownloadLlamaServer(
  root: string,
): Promise<string | null> {
  const binaryName = llamaServerBinaryName();
  const versionDir = engineVersionDir(root, LLAMA_CPP_ENGINE_TAG);

  // Cache hit: a previously extracted binary under engine/<tag>/.
  const cached = await findExecutableUnder(versionDir, binaryName);
  if (cached) {
    try {
      await fs.access(cached, fsConstants.X_OK);
      return cached;
    } catch {
      // Present but not executable — fall through to re-resolve below.
    }
  }

  const key = `${process.platform}-${process.arch}`;
  const assetSpec = LLAMA_CPP_ENGINE_ASSETS[key];
  if (!assetSpec) return null;

  // Touch engineRoot so the directory exists / path-guard is exercised even
  // when the downloader is a test seam.
  await fs.mkdir(engineRoot(root), { recursive: true, mode: 0o700 });

  const spec: LlamaEngineDownloadSpec = {
    url: `https://github.com/ggml-org/llama.cpp/releases/download/${LLAMA_CPP_ENGINE_TAG}/${assetSpec.asset}`,
    asset: assetSpec.asset,
    sha256: assetSpec.sha256,
    tmpPath: tempPath(root, makeDownloadId("llama-engine")),
    versionDir,
    binaryName,
  };
  // Test seam stays pure (no progress arg). Production streams + reports byte
  // progress into the engine-ensure state machine. The `engineState` write here is the single source the
  // `localModel/engine/status` poll reads.
  if (engineDownloaderOverride) {
    return await engineDownloaderOverride(spec);
  }
  return await defaultEngineDownload(spec, (received, total) => {
    engineState =
      total != null && total > 0 && received >= total
        ? { state: "extracting", receivedBytes: received, totalBytes: total }
        : {
            state: "downloading",
            receivedBytes: received,
            ...(total != null ? { totalBytes: total } : {}),
          };
  });
}

/**
 * Build the current engine snapshot. The on-disk cache is authoritative for
 * `ready` (a binary present from a prior session); platform-asset absence is
 * authoritative for `unavailable`; otherwise reflect the live ensure state
 * (`downloading` progress / `failed`) or `unknown` when no ensure ran yet.
 */
async function computeEngineStatus(
  root: string,
): Promise<WorkerLocalModelEngineStatus> {
  const tag = LLAMA_CPP_ENGINE_TAG;
  // A user-provided `CRABCODE_LLAMA_SERVER` binary short-circuits the whole
  // auto-download path in `server/start`, so it must report `ready` here too —
  // otherwise the wizard would block run on a download it never needs.
  const envBinary = process.env.CRABCODE_LLAMA_SERVER;
  if (envBinary != null && envBinary.trim().length > 0) {
    return { state: "ready", tag, binaryPath: envBinary.trim() };
  }
  const binaryName = llamaServerBinaryName();
  const versionDir = engineVersionDir(root, tag);
  const cached = await findExecutableUnder(versionDir, binaryName);
  if (cached) {
    engineState = { state: "ready", binaryPath: cached };
    return { state: "ready", tag, binaryPath: cached };
  }
  const key = `${process.platform}-${process.arch}`;
  if (!LLAMA_CPP_ENGINE_ASSETS[key]) {
    return { state: "unavailable", tag, reason: "no-prebuilt-engine-for-platform" };
  }
  switch (engineState.state) {
    case "downloading":
    case "extracting":
      return {
        state: engineState.state,
        tag,
        ...(engineState.receivedBytes != null
          ? { receivedBytes: engineState.receivedBytes }
          : {}),
        ...(engineState.totalBytes != null
          ? { totalBytes: engineState.totalBytes }
          : {}),
      };
    case "failed":
      return {
        state: "failed",
        tag,
        ...(engineState.error != null ? { error: engineState.error } : {}),
      };
    default:
      // `ready` here would mean cache lookup just failed despite a prior
      // success (binary removed mid-session) — fall back to `unknown` so a
      // fresh ensure can re-resolve.
      return { state: "unknown", tag };
  }
}

/**
 * Single-flight engine resolver. Returns the resolved binary path (or null when
 * the platform has no prebuilt asset). Concurrent callers — `engine/ensure`
 * (background, non-blocking) and `server/start` (awaits) — join the SAME
 * in-flight download, so the engine is never fetched twice. Errors are recorded
 * into `engineState` AND re-thrown so awaiting callers (server/start) can map
 * them to `engine-download-failed`.
 */
function startEngineEnsure(root: string): Promise<string | null> {
  if (engineEnsureInflight) return engineEnsureInflight;
  // Optimistic synchronous transition so a status poll immediately after the
  // ensure call reflects activity (a cache-hit is corrected to `ready` by
  // `resolveOrDownloadLlamaServer` returning right away; `computeEngineStatus`
  // re-checks the cache first regardless).
  engineState = { state: "downloading", receivedBytes: 0 };
  const promise = (async (): Promise<string | null> => {
    try {
      const binary = await resolveOrDownloadLlamaServer(root);
      engineState = binary
        ? { state: "ready", binaryPath: binary }
        : { state: "unavailable", reason: "no-prebuilt-engine-for-platform" };
      return binary;
    } catch (error) {
      engineState = {
        state: "failed",
        error: error instanceof Error ? error.message : String(error),
      };
      throw error;
    } finally {
      engineEnsureInflight = null;
    }
  })();
  engineEnsureInflight = promise;
  return promise;
}

async function fileExists(path: string): Promise<boolean> {
  try {
    await fs.access(path);
    return true;
  } catch {
    return false;
  }
}

async function readInstalledMetadata(
  root: string,
  modelId: string,
): Promise<InstalledLocalModelMetadata | null> {
  const path = metadataPath(root, modelId);
  try {
    const parsed = JSON.parse(await fs.readFile(path, "utf8")) as Partial<
      InstalledLocalModelMetadata
    >;
    if (
      parsed.modelId !== modelId ||
      typeof parsed.artifactPath !== "string" ||
      typeof parsed.sha256 !== "string" ||
      typeof parsed.sizeBytes !== "number"
    ) {
      return null;
    }
    return {
      modelId,
      artifactPath: ensureInsideRoot(root, parsed.artifactPath),
      sha256: parsed.sha256,
      sizeBytes: parsed.sizeBytes,
      installedAt:
        typeof parsed.installedAt === "string" ? parsed.installedAt : "",
    };
  } catch {
    return null;
  }
}

function catalogEntries(): WorkerLocalModelCuratedEntry[] {
  return curatedCatalogOverride ?? [];
}

// ---- BYO-local (Bring-Your-Own GGUF) registry ----

/**
 * Read the BYO registry. Tolerant: a missing / unparsable / non-array file
 * yields an empty list rather than throwing — a corrupt registry must not brick
 * catalog reads or server starts.
 */
async function readByoRegistry(root: string): Promise<WorkerLocalModelByoRecord[]> {
  const path = byoRegistryPath(root);
  let raw: string;
  try {
    raw = await fs.readFile(path, "utf8");
  } catch {
    return [];
  }
  let parsed: unknown;
  try {
    parsed = JSON.parse(raw);
  } catch {
    return [];
  }
  if (!Array.isArray(parsed)) return [];
  const out: WorkerLocalModelByoRecord[] = [];
  for (const item of parsed) {
    if (
      isRecord(item) &&
      typeof item.id === "string" &&
      item.id.length > 0 &&
      typeof item.displayName === "string" &&
      typeof item.ggufPath === "string" &&
      item.ggufPath.length > 0 &&
      typeof item.sizeBytes === "number" &&
      typeof item.addedAtMs === "number"
    ) {
      out.push({
        id: item.id,
        displayName: item.displayName,
        ggufPath: item.ggufPath,
        sizeBytes: item.sizeBytes,
        addedAtMs: item.addedAtMs,
      });
    }
  }
  return out;
}

/** Atomic write of the BYO registry (tmp + rename inside root). */
async function writeByoRegistry(
  root: string,
  records: WorkerLocalModelByoRecord[],
): Promise<void> {
  const path = byoRegistryPath(root);
  const tmp = ensureInsideRoot(root, join(root, "byo", `.registry-${makeDownloadId("byo")}.tmp`));
  await fs.mkdir(dirname(path), { recursive: true, mode: 0o700 });
  await fs.writeFile(tmp, JSON.stringify(records, null, 2), { mode: 0o600 });
  await fs.rename(tmp, path);
}

/** Derive a safe, validateModelId-compatible slug from a name or file name. */
function deriveByoSlug(seed: string): string {
  const base = seed.replace(/\.gguf$/i, "");
  let slug = base
    .toLowerCase()
    .replace(/[^a-z0-9._-]+/g, "-")
    .replace(/^[^a-z0-9]+/, "")
    .replace(/-+/g, "-")
    .replace(/-+$/, "")
    .slice(0, 96);
  if (slug.length === 0) slug = "byo-model";
  return slug;
}

/** Ensure the derived slug does not collide with an existing BYO id. */
function dedupeByoId(slug: string, existing: WorkerLocalModelByoRecord[]): string {
  const taken = new Set(existing.map((r) => r.id));
  if (!taken.has(slug)) return slug;
  for (let i = 2; i < 10_000; i += 1) {
    const candidate = `${slug}-${i}`.slice(0, 127);
    if (!taken.has(candidate)) return candidate;
  }
  // Extremely unlikely; fall back to a time-based suffix.
  return `${slug}-${Date.now().toString(36)}`.slice(0, 127);
}

/** Probe a candidate BYO `.gguf` path for existence / type / readability / size. */
async function probeByoGguf(path: string): Promise<ByoGgufProbe> {
  if (byoGgufProbeOverride) return byoGgufProbeOverride(path);
  let stat: import("node:fs").Stats;
  try {
    stat = await fs.stat(path);
  } catch {
    return { exists: false, isFile: false, readable: false, sizeBytes: 0 };
  }
  let readable = false;
  try {
    const constants = (await import("node:fs")).constants;
    await fs.access(path, constants.R_OK);
    readable = true;
  } catch {
    readable = false;
  }
  return {
    exists: true,
    isFile: stat.isFile(),
    readable,
    sizeBytes: stat.size,
  };
}

/**
 * Build the catalog entry returned for a BYO record. BYO models are referenced
 * in place and never downloaded, so `sha256` is `null`: there is no remote
 * artifact to verify (sha256 only protects curated remote downloads). They are
 * `installed: true` immediately (the file is already on disk). The wire shape
 * conforms to the ts-rs-generated `LocalModelCatalogEntry`
 * (`source: "user-local-path"`, richer `LocalModelCatalogStatus`).
 */
function byoRecordToCatalogEntry(
  record: WorkerLocalModelByoRecord,
): WorkerLocalModelMergedCatalogEntry {
  return {
    id: record.id,
    displayName: record.displayName,
    description: null,
    runtime: "llama-server",
    protocol: "openai-compatible",
    format: "gguf",
    source: "user-local-path",
    license: "user-provided",
    sizeBytes: record.sizeBytes,
    // sha256 N/A for BYO: referenced-in-place file, no download to verify.
    sha256: null,
    installed: true,
    status: "installed",
    modelPath: record.ggufPath,
    reason: null,
  };
}

/** Widen a curated entry to the merged wire shape (adds nullable fields). */
function curatedToMergedEntry(
  entry: WorkerLocalModelCatalogEntry,
): WorkerLocalModelMergedCatalogEntry {
  return {
    id: entry.id,
    displayName: entry.displayName,
    description: entry.description,
    runtime: entry.runtime,
    protocol: entry.protocol,
    format: entry.format,
    source: entry.source,
    license: entry.license,
    sizeBytes: entry.sizeBytes,
    sha256: entry.sha256,
    installed: entry.installed,
    status: entry.status,
    modelPath: null,
    reason: null,
  };
}

function toCatalogEntry(
  entry: WorkerLocalModelCuratedEntry,
  installed: boolean,
): WorkerLocalModelCatalogEntry {
  return {
    id: entry.id,
    displayName: entry.displayName,
    description: entry.description,
    runtime: entry.runtime,
    protocol: entry.protocol,
    format: entry.format,
    source: entry.source,
    license: entry.license,
    sizeBytes: entry.sizeBytes,
    sha256: entry.sha256,
    installed,
    status: installed ? "installed" : "not-installed",
  };
}

function sourcePathFromCuratedEntry(entry: WorkerLocalModelCuratedEntry): string {
  const url = new URL(entry.sourceUri);
  if (url.protocol !== "file:") {
    throw new WorkerError(
      INVALID_PARAMS,
      "curated local model source must be file:// in this window",
    );
  }
  return fileURLToPath(url);
}

function artifactFileName(entry: WorkerLocalModelCuratedEntry): string {
  const raw = entry.fileName ?? basename(fileURLToPath(new URL(entry.sourceUri)));
  const name = basename(raw);
  if (!/^[A-Za-z0-9][A-Za-z0-9._-]{0,191}$/.test(name)) {
    throw new WorkerError(
      INVALID_PARAMS,
      "curated local model file name must be safe",
    );
  }
  return name;
}

function sha256(buffer: Buffer): string {
  return createHash("sha256").update(buffer).digest("hex");
}

function makeDownloadId(modelId: string): string {
  return `${modelId}-${Date.now().toString(36)}-${Math.random()
    .toString(36)
    .slice(2, 10)}`;
}

function blockedDownloadResult(
  modelId: string,
  reason: string,
): WorkerLocalModelDownloadResult {
  return {
    status: {
      state: "not-found",
      reason,
      downloadId: null,
      modelId,
      bytesReceived: 0,
      totalBytes: null,
      percentage: null,
      error: null,
    },
  };
}

function unknownDownloadResult(
  downloadId: string | null,
  modelId: string | null = null,
): WorkerLocalModelDownloadResult {
  return {
    status: {
      state: "not-found",
      reason: "unknown-download",
      downloadId,
      modelId,
      bytesReceived: 0,
      totalBytes: null,
      percentage: null,
      error: null,
    },
  };
}

function findDownload(
  locator: { downloadId?: string; modelId?: string },
): InternalDownloadState | null {
  if (locator.downloadId) return downloads.get(locator.downloadId) ?? null;
  for (const download of downloads.values()) {
    if (download.status.modelId === locator.modelId) {
      return download;
    }
  }
  return null;
}

export async function localModelCatalogReadHandler(
  rawParams: unknown,
  _ctx: MethodContext,
): Promise<WorkerLocalModelCatalogReadResult> {
  validateEmptyObjectParams(rawParams);
  const root = resolveLocalModelsRoot();
  const curated = await Promise.all(
    catalogEntries().map(async (entry) => {
      validateModelId(entry.id);
      const installed = await readInstalledMetadata(root, entry.id);
      return curatedToMergedEntry(toCatalogEntry(entry, Boolean(installed)));
    }),
  );

  // Merge BYO (Bring-Your-Own GGUF) entries: user-registered local files,
  // referenced in place, already installed. These are the practical source of
  // usable local model entries while curated stays empty (license sign-off).
  const byo = await readByoRegistry(root);
  const byoData = byo.map(byoRecordToCatalogEntry);

  return {
    data: [...curated, ...byoData],
    // `source` is the catalog's dominant origin; with curated empty and BYO
    // present, report the BYO origin so callers don't mislabel user entries.
    source: byoData.length > 0 && curated.length === 0 ? "user-local-path" : "curated",
    manifestStatus: "awaiting-license-signoff",
    manifestVersion: 1,
  };
}

export async function localModelByoAddHandler(
  rawParams: unknown,
  _ctx: MethodContext,
): Promise<{ entry: WorkerLocalModelMergedCatalogEntry }> {
  if (!isRecord(rawParams)) {
    throw new WorkerError(INVALID_PARAMS, "params must be an object");
  }
  const ggufPathRaw = rawParams.ggufPath;
  if (typeof ggufPathRaw !== "string" || ggufPathRaw.length === 0) {
    throw new WorkerError(INVALID_PARAMS, "params.ggufPath must be a non-empty string");
  }
  if (!isAbsolute(ggufPathRaw)) {
    throw new WorkerError(INVALID_PARAMS, "params.ggufPath must be an absolute path");
  }
  const ggufPath = resolve(ggufPathRaw);
  if (!/\.gguf$/i.test(ggufPath)) {
    throw new WorkerError(INVALID_PARAMS, "params.ggufPath must point to a .gguf file");
  }
  const displayNameRaw = rawParams.displayName;
  if (
    displayNameRaw !== undefined &&
    displayNameRaw !== null &&
    (typeof displayNameRaw !== "string" || displayNameRaw.length === 0)
  ) {
    throw new WorkerError(
      INVALID_PARAMS,
      "params.displayName must be a non-empty string when provided",
    );
  }

  const probe = await probeByoGguf(ggufPath);
  if (!probe.exists) {
    throw new WorkerError(INVALID_PARAMS, "gguf file does not exist at the given path");
  }
  if (!probe.isFile) {
    throw new WorkerError(INVALID_PARAMS, "gguf path is not a regular file");
  }
  if (!probe.readable) {
    throw new WorkerError(INVALID_PARAMS, "gguf file is not readable");
  }

  const root = resolveLocalModelsRoot();
  const registry = await readByoRegistry(root);

  const displayName =
    typeof displayNameRaw === "string" ? displayNameRaw : basename(ggufPath);
  const slugSeed =
    typeof displayNameRaw === "string" ? displayNameRaw : basename(ggufPath);
  const id = dedupeByoId(deriveByoSlug(slugSeed), registry);
  validateModelId(id);

  const record: WorkerLocalModelByoRecord = {
    id,
    displayName,
    ggufPath,
    sizeBytes: probe.sizeBytes,
    addedAtMs: Date.now(),
  };
  await writeByoRegistry(root, [...registry, record]);

  return { entry: byoRecordToCatalogEntry(record) };
}

export async function localModelByoRemoveHandler(
  rawParams: unknown,
  _ctx: MethodContext,
): Promise<{ removed: boolean }> {
  if (!isRecord(rawParams)) {
    throw new WorkerError(INVALID_PARAMS, "params must be an object");
  }
  const id = rawParams.id;
  if (typeof id !== "string" || id.length === 0) {
    throw new WorkerError(INVALID_PARAMS, "params.id must be a non-empty string");
  }
  const root = resolveLocalModelsRoot();
  const registry = await readByoRegistry(root);
  const next = registry.filter((r) => r.id !== id);
  if (next.length === registry.length) {
    return { removed: false };
  }
  // De-register only — the referenced GGUF file is never deleted (we never
  // copied it; the user owns that file).
  await writeByoRegistry(root, next);
  return { removed: true };
}

/** Resolve a BYO entry's external `.gguf` path by id, or null when not BYO. */
async function resolveByoModelPath(
  root: string,
  modelId: string,
): Promise<string | null> {
  const registry = await readByoRegistry(root);
  const record = registry.find((r) => r.id === modelId);
  return record ? record.ggufPath : null;
}

export async function localModelSystemProfileReadHandler(
  rawParams: unknown,
  _ctx: MethodContext,
): Promise<WorkerLocalModelSystemProfileResult> {
  validateEmptyObjectParams(rawParams);

  const profile = readSystemProfile();
  const runtimeSupport = resolveRuntimeSupport(profile);
  return {
    platform: profile.platform,
    arch: profile.arch,
    memoryBytes: profile.memoryBytes,
    ...runtimeSupport,
  };
}

export async function localModelDownloadStartHandler(
  rawParams: unknown,
  _ctx: MethodContext,
): Promise<WorkerLocalModelDownloadResult> {
  if (workerRuntimeShuttingDown) {
    throw new WorkerError(INVALID_PARAMS, "worker-runtime-shutting-down");
  }
  const modelId = requireModelId(rawParams);
  const entitlementFns = await loadLocalModelEntitlementFns();
  if (!entitlementFns.canUseLocalModels(entitlementFns.getMembershipGateInput())) {
    throw new WorkerError(INVALID_PARAMS, LOCAL_MODEL_ENTITLEMENT_ERROR);
  }
  const entry = catalogEntries().find((candidate) => candidate.id === modelId);
  if (!entry) {
    return blockedDownloadResult(modelId, "catalog-entry-not-signed-or-not-found");
  }

  const root = resolveLocalModelsRoot();
  const downloadId = makeDownloadId(modelId);
  const tmpPath = tempPath(root, downloadId);
  const targetDir = modelDir(root, modelId);
  const targetPath = ensureInsideRoot(root, join(targetDir, artifactFileName(entry)));
  const state: InternalDownloadState = {
    status: {
      state: "downloading",
      reason: null,
      downloadId,
      modelId,
      bytesReceived: 0,
      totalBytes: null,
      percentage: null,
      error: null,
    },
    artifactPath: null,
    sha256: null,
  };
  downloads.set(downloadId, state);

  try {
    const sourcePath = sourcePathFromCuratedEntry(entry);
    const payload = await fs.readFile(sourcePath);
    const digest = sha256(payload);
    state.status.bytesReceived = payload.byteLength;
    state.status.totalBytes = payload.byteLength;
    state.status.percentage = 100;
    state.sha256 = digest;

    await fs.mkdir(dirname(tmpPath), { recursive: true, mode: 0o700 });
    await fs.mkdir(targetDir, { recursive: true, mode: 0o700 });
    await fs.writeFile(tmpPath, payload, { mode: 0o600 });

    if (digest !== entry.sha256) {
      await fs.rm(tmpPath, { force: true });
      state.status.state = "failed";
      state.status.reason = "sha256-mismatch";
      state.status.error = "sha256-mismatch";
      return { status: state.status };
    }

    await fs.rename(tmpPath, targetPath);
    const metadata: InstalledLocalModelMetadata = {
      modelId,
      artifactPath: targetPath,
      sha256: digest,
      sizeBytes: payload.byteLength,
      installedAt: new Date().toISOString(),
    };
    const metaPath = metadataPath(root, modelId);
    await fs.mkdir(dirname(metaPath), { recursive: true, mode: 0o700 });
    await fs.writeFile(metaPath, JSON.stringify(metadata, null, 2), {
      mode: 0o600,
    });

    state.status.state = "completed";
    state.status.reason = null;
    state.artifactPath = targetPath;
    return { status: state.status };
  } catch (error) {
    await fs.rm(tmpPath, { force: true });
    state.status.state = "failed";
    state.status.reason =
      error instanceof WorkerError ? error.message : "download-failed";
    state.status.error = state.status.reason;
    state.artifactPath = null;
    return { status: state.status };
  }
}

export async function localModelDownloadProgressHandler(
  rawParams: unknown,
  _ctx: MethodContext,
): Promise<WorkerLocalModelDownloadResult> {
  const locator = readDownloadLocator(rawParams);
  const current = findDownload(locator);
  return current
    ? { status: current.status }
    : unknownDownloadResult(locator.downloadId ?? null, locator.modelId ?? null);
}

export async function localModelDownloadCancelHandler(
  rawParams: unknown,
  _ctx: MethodContext,
): Promise<WorkerLocalModelDownloadResult> {
  const locator = readDownloadLocator(rawParams);
  const current = findDownload(locator);
  if (!current) {
    return unknownDownloadResult(
      locator.downloadId ?? null,
      locator.modelId ?? null,
    );
  }
  if (
    current.status.state === "completed" ||
    current.status.state === "failed"
  ) {
    return { status: current.status };
  }
  current.status.state = "cancelled";
  current.status.reason = "cancelled";
  return { status: current.status };
}

export async function localModelInstallRemoveHandler(
  rawParams: unknown,
  _ctx: MethodContext,
): Promise<WorkerLocalModelInstallRemoveResult> {
  const modelId = requireModelId(rawParams);
  const root = resolveLocalModelsRoot();
  const metadata = await readInstalledMetadata(root, modelId);
  if (!metadata || !(await fileExists(metadata.artifactPath))) {
    await fs.rm(metadataPath(root, modelId), { force: true });
    return {
      state: "not-found",
      reason: "not-installed",
      modelId,
      modelPath: null,
    };
  }

  await fs.rm(metadata.artifactPath, { force: true });
  await fs.rm(modelDir(root, modelId), { recursive: true, force: true });
  await fs.rm(metadataPath(root, modelId), { force: true });
  return {
    state: "removed",
    reason: null,
    modelId,
    modelPath: metadata.artifactPath,
  };
}

// ---- PR-7D + BLOCKER-3: server lifecycle handlers ----

function resolveServerTimings(): LocalModelServerTimings {
  return { ...DEFAULT_SERVER_TIMINGS, ...(serverTimingsOverride ?? {}) };
}

function delay(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

/** Default production `/health` readiness probe. */
async function defaultHealthProbe(
  url: string,
  signal: AbortSignal,
): Promise<boolean> {
  try {
    const base = url.replace(/\/+$/, "");
    const res = await fetch(`${base}/health`, { signal });
    return res.ok;
  } catch {
    return false;
  }
}

/**
 * BLOCKER-3: reserve a free loopback TCP port instead of guessing a fixed
 * default. Binds `:0` on the loopback host, reads the OS-assigned port, and
 * releases it. A small TOCTOU window remains (another process could grab the
 * port before llama-server binds it); that is acceptable and matches how most
 * local tooling reserves ports.
 */
async function defaultReserveLoopbackPort(): Promise<number> {
  const net = await import("node:net");
  return await new Promise<number>((resolve, reject) => {
    const srv = net.createServer();
    srv.once("error", reject);
    srv.listen(0, LOOPBACK_HOST, () => {
      const addr = srv.address();
      const port = addr && typeof addr === "object" ? addr.port : 0;
      srv.close((err) => (err ? reject(err) : resolve(port)));
    });
  });
}

/**
 * Default production process starter. Wraps `Bun.spawn`, keeps the
 * `Bun.Subprocess` handle alive behind the `LocalModelServerProcess`
 * interface, and drains stderr into a bounded ring buffer so a failed start
 * can surface a diagnostic tail.
 */
async function defaultProductionStart(
  args: LocalModelServerStarterArgs,
): Promise<LocalModelServerProcess> {
  const proc = Bun.spawn([args.binary, ...args.argv], {
    stdout: "ignore",
    stderr: "pipe",
  });
  const pid = proc.pid;
  if (typeof pid !== "number" || pid <= 0) {
    try {
      proc.kill();
    } catch {
      /* nothing to clean up */
    }
    throw new Error("llama-server spawn failed: no valid pid");
  }

  const STDERR_TAIL_LIMIT = 4096;
  let stderrTail = "";
  const stderrStream = proc.stderr as unknown;
  if (
    stderrStream &&
    typeof (stderrStream as ReadableStream<Uint8Array>).getReader === "function"
  ) {
    void (async () => {
      try {
        const reader = (stderrStream as ReadableStream<Uint8Array>).getReader();
        const decoder = new TextDecoder();
        for (;;) {
          const { done, value } = await reader.read();
          if (done) break;
          stderrTail = (stderrTail + decoder.decode(value, { stream: true })).slice(
            -STDERR_TAIL_LIMIT,
          );
        }
      } catch {
        /* stream closed / process gone */
      }
    })();
  }

  return {
    pid,
    exited: proc.exited.then((code) => (typeof code === "number" ? code : null)),
    kill: (signal) => {
      try {
        proc.kill(signal as number | undefined);
      } catch {
        /* already gone */
      }
    },
    readStderrTail: () => stderrTail,
  };
}

/**
 * BLOCKER-3: graceful stop with SIGKILL escalation. SIGTERM first, wait up to
 * the grace period, then SIGKILL if the process has not exited. Clears
 * `currentServer` and returns the observed exit code.
 */
async function stopCurrentServerProcess(reason: string): Promise<number | null> {
  if (!currentServer) return null;
  const server = currentServer;
  const proc = server.process;
  // Mark `stopping` first so the crash-monitor callback does not reclassify
  // the process exit as a `failed` crash.
  currentServer = { ...server, state: "stopping", reason };

  if (!proc) {
    currentServer = null;
    return server.exitCode ?? null;
  }

  const timings = resolveServerTimings();
  proc.kill("SIGTERM");
  const settled = await Promise.race([
    proc.exited.then((code) => ({ done: true as const, code })),
    delay(timings.stopGraceMs).then(() => ({
      done: false as const,
      code: null as number | null,
    })),
  ]);

  let exitCode: number | null;
  if (settled.done) {
    exitCode = settled.code;
  } else {
    // SIGTERM did not land within the grace period — escalate to SIGKILL.
    proc.kill("SIGKILL");
    // Do not turn a rejected/absent exit witness into "stopped". The worker
    // shutdown RPC must fail (and the Rust host must retain authority) unless
    // the exact process handle confirms termination.
    exitCode = await proc.exited;
  }
  currentServer = null;
  return exitCode;
}

function buildServerStatus(): WorkerLocalModelServerStatus {
  if (!currentServer) return { state: "stopped" };
  const s = currentServer;
  const status: WorkerLocalModelServerStatus = { state: s.state };
  if (s.reason !== undefined) status.reason = s.reason;
  if (s.host) status.host = s.host;
  if (s.port !== undefined) status.port = s.port;
  if (s.url) status.url = s.url;
  if (s.pid !== undefined) status.pid = s.pid;
  if (s.modelId !== undefined) status.modelId = s.modelId;
  if (s.modelPath) status.modelPath = s.modelPath;
  if (s.error !== undefined) status.error = s.error;
  if (s.stderrTail !== undefined) status.stderrTail = s.stderrTail;
  return status;
}

interface ServerStartParamsParsed {
  modelId?: string;
  modelPath?: string;
  /** Explicit port, or `undefined` to reserve a free loopback port. */
  port?: number;
  contextSize?: number;
  gpuLayers?: number;
  /** Which local engine to launch; defaults to `llama-server` when absent. */
  runtime?: LocalModelRuntime;
}

function readServerStartParams(raw: unknown): ServerStartParamsParsed {
  if (!isRecord(raw)) {
    throw new WorkerError(INVALID_PARAMS, "params must be an object");
  }
  const { modelId, modelPath, port, contextSize, gpuLayers, runtime } = raw;

  if (modelId !== undefined && (typeof modelId !== "string" || modelId.length === 0)) {
    throw new WorkerError(INVALID_PARAMS, "params.modelId must be a non-empty string when provided");
  }
  if (modelPath !== undefined && (typeof modelPath !== "string" || modelPath.length === 0)) {
    throw new WorkerError(INVALID_PARAMS, "params.modelPath must be a non-empty string when provided");
  }
  if (!modelId && !modelPath) {
    throw new WorkerError(INVALID_PARAMS, "params.modelId or params.modelPath is required");
  }
  if (typeof modelId === "string") validateModelId(modelId);

  // BLOCKER-3: an absent port (or an explicit 0) means "reserve a free
  // loopback port" — the handler resolves it below. Only 1..65535 pins an
  // explicit port; anything else is rejected.
  let resolvedPort: number | undefined;
  if (port !== undefined && port !== null) {
    if (typeof port !== "number" || !Number.isInteger(port) || port < 0 || port > 65535) {
      throw new WorkerError(INVALID_PARAMS, "params.port must be a valid port number (0-65535)");
    }
    resolvedPort = port === 0 ? undefined : port;
  }

  let resolvedContextSize: number | undefined;
  if (contextSize !== undefined) {
    if (typeof contextSize !== "number" || !Number.isInteger(contextSize) || contextSize < 1) {
      throw new WorkerError(INVALID_PARAMS, "params.contextSize must be a positive integer");
    }
    resolvedContextSize = contextSize;
  }

  let resolvedGpuLayers: number | undefined;
  if (gpuLayers !== undefined) {
    if (typeof gpuLayers !== "number" || !Number.isInteger(gpuLayers) || gpuLayers < 0) {
      throw new WorkerError(INVALID_PARAMS, "params.gpuLayers must be a non-negative integer");
    }
    resolvedGpuLayers = gpuLayers;
  }

  let resolvedRuntime: LocalModelRuntime | undefined;
  if (runtime !== undefined && runtime !== null) {
    if (typeof runtime !== "string" || !isKnownRuntime(runtime)) {
      throw new WorkerError(
        INVALID_PARAMS,
        `params.runtime must be ${Object.keys(ENGINE_SPECS)
          .map((r) => `'${r}'`)
          .join(" or ")} when provided`,
      );
    }
    resolvedRuntime = runtime;
  }

  return {
    modelId: typeof modelId === "string" ? modelId : undefined,
    modelPath: typeof modelPath === "string" ? modelPath : undefined,
    port: resolvedPort,
    contextSize: resolvedContextSize,
    gpuLayers: resolvedGpuLayers,
    runtime: resolvedRuntime,
  };
}

export async function localModelServerStartHandler(
  rawParams: unknown,
  _ctx: MethodContext,
): Promise<WorkerLocalModelServerResult> {
  if (workerRuntimeShuttingDown) {
    throw new WorkerError(INVALID_PARAMS, "worker-runtime-shutting-down");
  }
  const params = readServerStartParams(rawParams);
  const entitlementFns = await loadLocalModelEntitlementFns();
  if (!entitlementFns.canUseLocalModels(entitlementFns.getMembershipGateInput())) {
    throw new WorkerError(INVALID_PARAMS, LOCAL_MODEL_ENTITLEMENT_ERROR);
  }

  // Resolve model file path
  let resolvedModelPath: string;
  let resolvedModelId: string | undefined;

  if (params.modelId) {
    const root = resolveLocalModelsRoot();
    // BYO entries reference an external `.gguf` path (not under models/{id}/).
    // Resolve those first; fall back to the curated install metadata path.
    const byoPath = await resolveByoModelPath(root, params.modelId);
    if (byoPath !== null) {
      // Re-validate the referenced file through the same probe seam used by
      // byo/add (the file lives outside the local-models root and the user may
      // have moved / deleted it since registration).
      const probe = await probeByoGguf(byoPath);
      if (!probe.exists || !probe.isFile) {
        return {
          status: {
            state: "failed",
            reason: "model-file-not-found",
            modelId: params.modelId,
            modelPath: byoPath,
            error: "byo gguf file no longer exists at registered path",
          },
        };
      }
      resolvedModelPath = byoPath;
      resolvedModelId = params.modelId;
    } else {
      const metadata = await readInstalledMetadata(root, params.modelId);
      if (!metadata) {
        return {
          status: {
            state: "failed",
            reason: "model-not-installed",
            modelId: params.modelId,
            error: "model not installed",
          },
        };
      }
      resolvedModelPath = metadata.artifactPath;
      resolvedModelId = params.modelId;
    }
  } else {
    const mp = params.modelPath!;
    if (!isAbsolute(mp)) {
      throw new WorkerError(INVALID_PARAMS, "params.modelPath must be an absolute path");
    }
    if (!(await fileExists(mp))) {
      return {
        status: {
          state: "failed",
          reason: "model-file-not-found",
          modelPath: mp,
          error: "model file not found at path",
        },
      };
    }
    resolvedModelPath = resolve(mp);
  }

  // Engine selection. `ServerStartParams.runtime` (optional; defaults to
  // `llama-server`) decides which binary to launch. ds4 is the user-installed
  // antirez DeepSeek V4 Flash engine resolved from `CRABCODE_DS4_SERVER`; it
  // speaks the same OpenAI-compatible loopback protocol, so only binary
  // resolution differs — the loopback / health / lifecycle path below is shared.
  const starter = serverStarterOverride;
  // Engine differences live in ENGINE_SPECS — read the spec for the requested
  // runtime instead of branching on `useDs4` here and in the three sites below.
  // An absent runtime defaults to llama-server (the pre-existing default).
  const engineSpec = ENGINE_SPECS[params.runtime ?? "llama-server"];
  const binaryFromEnv = process.env[engineSpec.binaryEnvVar];
  // On-demand engine resolution: when this is a llama-server start with neither
  // an injected starter nor an env binary, try to reuse / download the official
  // llama.cpp pre-built `llama-server` for this platform before giving up. ds4
  // is user-installed only (no auto-download — keep `missing-ds4-server`).
  let downloadedEngineBinary: string | null = null;
  if (
    !starter &&
    engineSpec.autoDownload &&
    (!binaryFromEnv || binaryFromEnv.trim().length === 0)
  ) {
    try {
      const root = resolveLocalModelsRoot();
      // Single-flight: joins any in-flight `localModel/engine/ensure` download
      // instead of starting a second one. Bounded by the caller's 15s timeout;
      // on a slow first download the dispatcher may time out while the
      // background ensure keeps running — the wizard's engine/status poll then
      // shows progress and the user retries once ready.
      downloadedEngineBinary = await startEngineEnsure(root);
    } catch (error) {
      // Download / checksum / extract failure — fail soft (never crash
      // server/start). Surface a distinct `engine-download-failed` reason.
      const errMsg = error instanceof Error ? error.message : String(error);
      return {
        status: {
          state: "unavailable",
          reason: "engine-download-failed",
          error: errMsg,
        },
      };
    }
  }
  if (
    !starter &&
    !downloadedEngineBinary &&
    (!binaryFromEnv || binaryFromEnv.trim().length === 0)
  ) {
    return {
      status: {
        state: "unavailable",
        reason: engineSpec.missingBinaryReason,
      },
    };
  }

  // BLOCKER-3: a second start must stop the previous process first — the old
  // implementation overwrote `currentServer` and orphaned the old child.
  if (currentServer) {
    await stopCurrentServerProcess("superseded-by-new-start");
  }

  // BLOCKER-3: reserve a free loopback port when the caller did not pin one,
  // instead of guessing a fixed 8080 that may already be in use.
  let port: number;
  if (params.port !== undefined) {
    port = params.port;
  } else {
    try {
      port = await (portReserverOverride ?? defaultReserveLoopbackPort)();
    } catch (error) {
      const errMsg = error instanceof Error ? error.message : String(error);
      currentServer = {
        state: "failed",
        host: LOOPBACK_HOST,
        port: 0,
        url: "",
        modelId: resolvedModelId,
        modelPath: resolvedModelPath,
        reason: "port-reservation-failed",
        error: errMsg,
        generation: ++serverGeneration,
      };
      return { status: buildServerStatus() };
    }
  }

  const binary =
    binaryFromEnv ?? downloadedEngineBinary ?? engineSpec.fallbackBinaryName;
  const url = `http://${LOOPBACK_HOST}:${port}`;
  const generation = ++serverGeneration;

  // argv always binds loopback; caller-supplied host is ignored
  const argv: string[] = [
    "-m", resolvedModelPath,
    "--host", LOOPBACK_HOST,
    "--port", String(port),
  ];
  if (params.contextSize !== undefined) argv.push("--ctx-size", String(params.contextSize));
  if (params.gpuLayers !== undefined) argv.push("--n-gpu-layers", String(params.gpuLayers));

  currentServer = {
    state: "starting",
    host: LOOPBACK_HOST,
    port,
    url,
    modelId: resolvedModelId,
    modelPath: resolvedModelPath,
    generation,
  };

  let proc: LocalModelServerProcess;
  try {
    proc = await (starter ?? defaultProductionStart)({
      binary,
      argv,
      modelPath: resolvedModelPath,
      host: LOOPBACK_HOST,
      port,
    });
  } catch (error) {
    const errMsg = error instanceof Error ? error.message : String(error);
    currentServer = {
      state: "failed",
      host: LOOPBACK_HOST,
      port,
      url,
      modelId: resolvedModelId,
      modelPath: resolvedModelPath,
      error: errMsg,
      generation,
    };
    return { status: buildServerStatus() };
  }

  currentServer = { ...currentServer, pid: proc.pid, process: proc };

  // BLOCKER-3: monitor for a crash. When the process exits while we still own
  // `currentServer` and were not deliberately stopping it, reclassify as
  // `failed` with the exit code and a stderr tail.
  let processExited = false;
  void proc.exited.then((code) => {
    processExited = true;
    if (
      currentServer &&
      currentServer.generation === generation &&
      currentServer.state !== "stopping" &&
      currentServer.state !== "stopped"
    ) {
      currentServer = {
        ...currentServer,
        state: "failed",
        reason: "process-exited",
        error: `llama-server exited with code ${code ?? "(signal)"}`,
        exitCode: code,
        stderrTail: proc.readStderrTail?.() || currentServer.stderrTail,
      };
    }
  });

  // BLOCKER-3: wait for `/health` readiness so `running` is never reported
  // for a server that cannot answer yet. The budget is well under the 15s
  // dispatcher timeout; if the model is still loading we return an honest
  // `starting` and `server/status` flips it to `running` once ready.
  const timings = resolveServerTimings();
  const probe = healthProbeOverride ?? defaultHealthProbe;
  const deadline = Date.now() + timings.startHealthWaitMs;
  while (Date.now() < deadline) {
    if (processExited) {
      // The crash callback already reclassified `currentServer` as failed.
      return { status: buildServerStatus() };
    }
    const ac = new AbortController();
    const probeTimer = setTimeout(() => ac.abort(), timings.healthProbeTimeoutMs);
    let healthy = false;
    try {
      healthy = await probe(url, ac.signal);
    } catch {
      healthy = false;
    } finally {
      clearTimeout(probeTimer);
    }
    if (healthy) {
      if (currentServer && currentServer.generation === generation) {
        currentServer = { ...currentServer, state: "running", reason: undefined };
      }
      return { status: buildServerStatus() };
    }
    await delay(timings.healthPollIntervalMs);
  }

  // Process is alive but `/health` is not ready within the budget — honest
  // `starting`, not a fake `running`. Callers poll `server/status`.
  if (currentServer && currentServer.generation === generation && !processExited) {
    currentServer = { ...currentServer, state: "starting", reason: "awaiting-health-ready" };
  }
  return { status: buildServerStatus() };
}

interface ServerStopParamsParsed {
  modelId?: string;
  modelPath?: string;
}

function readServerStopParams(raw: unknown): ServerStopParamsParsed {
  if (raw === undefined || raw === null) return {};
  if (!isRecord(raw)) {
    throw new WorkerError(INVALID_PARAMS, "params must be an object");
  }
  const out: ServerStopParamsParsed = {};
  const { modelId, modelPath } = raw;
  if (modelId !== undefined && modelId !== null) {
    if (typeof modelId !== "string" || modelId.length === 0) {
      throw new WorkerError(INVALID_PARAMS, "params.modelId must be a non-empty string when provided");
    }
    validateModelId(modelId);
    out.modelId = modelId;
  }
  if (modelPath !== undefined && modelPath !== null) {
    if (typeof modelPath !== "string" || modelPath.length === 0) {
      throw new WorkerError(INVALID_PARAMS, "params.modelPath must be a non-empty string when provided");
    }
    out.modelPath = modelPath;
  }
  return out;
}

export async function localModelServerStopHandler(
  rawParams: unknown,
  _ctx: MethodContext,
): Promise<WorkerLocalModelServerResult> {
  readServerStopParams(rawParams);

  if (!currentServer) {
    return { status: { state: "stopped" } };
  }

  // BLOCKER-3: graceful SIGTERM → wait → SIGKILL escalation, with the exit
  // code reflected back. `stopCurrentServerProcess` handles every state:
  // a `failed` / `unavailable` server with no live process is just cleared.
  const exitCode = await stopCurrentServerProcess("client-requested-stop");
  const status: WorkerLocalModelServerStatus = { state: "stopped" };
  if (exitCode !== null && exitCode !== undefined) {
    status.reason = `exit-code-${exitCode}`;
  }
  return { status };
}

export async function localModelServerStatusHandler(
  rawParams: unknown,
  _ctx: MethodContext,
): Promise<WorkerLocalModelServerResult> {
  validateEmptyObjectParams(rawParams);
  if (!currentServer) {
    return { status: { state: "stopped" } };
  }

  // BLOCKER-3: status must verify liveness + health, not just echo memory.
  // The crash-monitor callback already flips an exited process to `failed`;
  // here we additionally run a live `/health` probe for running / starting
  // servers so a hung (but not exited) server is not reported as `running`.
  if (currentServer.state === "running" || currentServer.state === "starting") {
    const timings = resolveServerTimings();
    const probe = healthProbeOverride ?? defaultHealthProbe;
    const probeUrl = currentServer.url;
    const ac = new AbortController();
    const probeTimer = setTimeout(() => ac.abort(), timings.healthProbeTimeoutMs);
    let healthy = false;
    try {
      healthy = await probe(probeUrl, ac.signal);
    } catch {
      healthy = false;
    } finally {
      clearTimeout(probeTimer);
    }
    // Re-check `currentServer` — it may have been stopped / reclassified
    // during the await above.
    if (currentServer && currentServer.url === probeUrl) {
      if (healthy) {
        currentServer = { ...currentServer, state: "running", reason: undefined };
      } else if (currentServer.state === "running") {
        // Was running, no longer answering, but the process has not exited
        // (the crash callback would have set `failed`). Treat as transient.
        currentServer = {
          ...currentServer,
          state: "starting",
          reason: "health-check-failed",
        };
      }
    }
  }

  return { status: buildServerStatus() };
}

// ---- Engine ensure / status ----

export async function localModelEngineStatusHandler(
  rawParams: unknown,
  _ctx: MethodContext,
): Promise<WorkerLocalModelEngineResult> {
  validateEmptyObjectParams(rawParams);
  const root = resolveLocalModelsRoot();
  return { status: await computeEngineStatus(root) };
}

export async function localModelEngineEnsureHandler(
  rawParams: unknown,
  _ctx: MethodContext,
): Promise<WorkerLocalModelEngineResult> {
  if (workerRuntimeShuttingDown) {
    throw new WorkerError(INVALID_PARAMS, "worker-runtime-shutting-down");
  }
  validateEmptyObjectParams(rawParams);
  // Triggering a download is a privileged action — gate it like download/start
  // (status is a harmless read and stays ungated). Defense-in-depth beyond the
  // caller's own entitlement gate.
  const entitlementFns = await loadLocalModelEntitlementFns();
  if (
    !entitlementFns.canUseLocalModels(entitlementFns.getMembershipGateInput())
  ) {
    throw new WorkerError(INVALID_PARAMS, LOCAL_MODEL_ENTITLEMENT_ERROR);
  }
  const root = resolveLocalModelsRoot();
  const status = await computeEngineStatus(root);
  // Already resolved (cache hit) or no asset for this platform — nothing to do.
  if (status.state === "ready" || status.state === "unavailable") {
    return { status };
  }
  // Kick the non-blocking, single-flight background download. We do NOT await
  // it (the dispatcher call is bounded by a 15s timeout; the download is tens
  // of MB). Errors are recorded into `engineState` for the next status poll;
  // the `.catch` here only prevents an unhandled rejection.
  void startEngineEnsure(root).catch(() => undefined);
  return { status: await computeEngineStatus(root) };
}

export interface LocalModelProcessShutdownResult {
  engineEnsure: "idle" | "settled";
  server: "stopped";
}

export function beginLocalModelRuntimeProcessShutdown(): void {
  workerRuntimeShuttingDown = true;
}

/**
 * Settle every local-model resource that can outlive its initiating worker
 * RPC. New download/engine/server starts fail closed from the first line; an
 * already-running engine download/extract is awaited through its existing
 * single-flight promise, then the live inference server is TERM→KILL stopped
 * and its exact process exit witness is awaited.
 */
export async function shutdownLocalModelRuntimeForProcess(): Promise<LocalModelProcessShutdownResult> {
  beginLocalModelRuntimeProcessShutdown();
  const inflight = engineEnsureInflight;
  if (inflight) {
    try {
      await inflight;
    } catch {
      // startEngineEnsure records the terminal failure in engineState. A
      // rejected operation is still settled; only a pending one blocks ack.
    }
  }
  await stopCurrentServerProcess("worker-runtime-shutdown");
  return {
    engineEnsure: inflight ? "settled" : "idle",
    server: "stopped",
  };
}
