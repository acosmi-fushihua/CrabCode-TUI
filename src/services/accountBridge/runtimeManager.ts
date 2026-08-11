import {
  createHash,
  createHmac,
  createPublicKey,
  randomBytes,
  verify as verifySignature,
} from "node:crypto";
import { spawn } from "node:child_process";
import type { ChildProcess, StdioOptions } from "node:child_process";
import * as fs from "node:fs/promises";
import { createReadStream } from "node:fs";
import { homedir, networkInterfaces } from "node:os";
import {
  basename,
  dirname,
  join,
  resolve,
  win32 as pathWin32,
} from "node:path";
import { createInterface } from "node:readline";
import { fileURLToPath } from "node:url";
import { compare as compareSemver, valid as validSemver } from "semver";
import {
  parseAccountBridgeAccountList,
  parseAccountBridgeAccountRemoveView,
  parseAccountBridgeConnectorList,
  parseAccountBridgeEligibilityView,
  parseAccountBridgeLoginCancelView,
  parseAccountBridgeLoginPollView,
  parseAccountBridgeLoginStartView,
  parseAccountBridgeModelList,
  parseAccountBridgeRuntimeAccess,
  parseAccountBridgeRuntimeView,
  parseAccountBridgeUsageRead,
  type AccountBridgeAccountRemoveView,
  type AccountBridgeAccountView,
  type AccountBridgeConnectorView,
  type AccountBridgeEligibilityView,
  type AccountBridgeLoginCancelView,
  type AccountBridgeLoginPollView,
  type AccountBridgeLoginStartView,
  type AccountBridgeModelRouteView,
  type AccountBridgeRuntimeAccess,
  type AccountBridgeRuntimeView,
  type AccountBridgeUsageSnapshot,
} from "./types.js";
import {
  AccountBridgeMasterKeyError,
  loadOrCreateAccountBridgeMasterKey,
} from "./masterKey.js";

const COMPONENT = "OAuthAPI-LLM";
const COMPONENT_MODULE = "github.com/acosmi/OAuthAPI-LLM";
const GRANT_AUDIENCE = "crabcode-account-bridge";
// Control-plane generations. "v1" = legacy aggregate gating over the four
// legacy connectors; "v2" = per-connector region gating over all seven.
// Both grant and directory pin one of these and must agree per response.
const GRANT_VERSION = "v1";
const GRANT_VERSION_V2 = "v2";
type ControlPlaneGeneration = typeof GRANT_VERSION | typeof GRANT_VERSION_V2;
const GRANT_MAX_TTL_MS = 5 * 60_000;
const GRANT_BACKGROUND_REFRESH_MS = 4 * 60_000;
const CRASH_WINDOW_MS = 10 * 60_000;
const CRASH_BACKOFF_MS = [1_000, 2_000, 4_000] as const;
const STOP_GRACE_MS = 2_000;
const HTTP_TIMEOUT_MS = 10_000;
const READY_TIMEOUT_MS = 10_000;
const FACADE_PATH = "/v0/account-bridge/internal";
const CONNECTOR_DIRECTORY_AUDIENCE = "crabcode-account-bridge-connectors";
// W-ACCOUNT-BRIDGE-REGION-CONNECTORS (2026-07-17): seven shipped connectors.
// The legacy four remain the exact "v1" control-plane generation; qwen/kimi/
// zai only exist in the "v2" generation (per-connector region gating).
const CONNECTOR_IDS = [
  "openai",
  "anthropic",
  "google",
  "xai",
  "qwen",
  "kimi",
  "zai",
] as const;
const LEGACY_CONNECTOR_IDS = ["openai", "anthropic", "google", "xai"] as const;
type ConnectorRegionPolicy = "global" | "non-cn";
/**
 * Client-baked region classes (defense in depth): 'non-cn' connectors are
 * blocked under a CN egress even if a (mis-issued) v2 grant lists them;
 * 'global' connectors are servable everywhere the grant lists them. The
 * signed v2 directory carries the authoritative `regionPolicy` per entry;
 * this map fills v1 directories (which predate the field) and the static
 * fallback catalog so the policy type stays total.
 */
const CONNECTOR_REGION_POLICY: Record<
  (typeof CONNECTOR_IDS)[number],
  ConnectorRegionPolicy
> = {
  openai: "non-cn",
  anthropic: "non-cn",
  google: "non-cn",
  xai: "non-cn",
  qwen: "global",
  kimi: "global",
  zai: "global",
};
const CONNECTOR_AUTH_MODES = ["browser", "device-code"] as const;
const CONNECTOR_DISPLAY_NAME_MAX_CODE_POINTS = 80;
const CONNECTOR_DISPLAY_NAME_FORBIDDEN = /[\p{Cc}\p{Cf}]/u;
const NETWORK_SIGNATURE_POLL_MS = 5_000;
const CONTROL_PLANE_MAX_BYTES = 128 << 10;
const LICENSE_MATERIALS_DIRECTORY = "third-party-licenses";
const LICENSE_MATERIALS_MANIFEST = "third-party-licenses.manifest.json";
const MAX_LICENSE_MATERIAL_FILES = 512;
const MAX_LICENSE_MATERIAL_FILE_BYTES = 2 * 1024 * 1024;
const MAX_LICENSE_MATERIAL_TOTAL_BYTES = 32 * 1024 * 1024;
const MAX_LICENSE_MATERIAL_PATH_BYTES = 1024;

// Source-free release builds replace these identifiers with Bun defines.
// `typeof` guards below deliberately keep raw-source tests fail-closed without
// accepting argv/env values at runtime.
declare const ACCOUNT_BRIDGE_COMPONENT_VERSION: unknown;
declare const ACCOUNT_BRIDGE_PROTOCOL_VERSION: unknown;
declare const ACCOUNT_BRIDGE_CRABCODE_RELEASE_VERSION: unknown;
declare const ACCOUNT_BRIDGE_ELIGIBILITY_PUBLIC_KEY_BASE64URL: unknown;
declare const ACCOUNT_BRIDGE_CONNECTOR_POLICY_PUBLIC_KEY_BASE64URL: unknown;
declare const ACCOUNT_BRIDGE_ARTIFACT_PUBLIC_KEY_BASE64URL: unknown;
declare const ACCOUNT_BRIDGE_CONTROL_PLANE_ENDPOINT: unknown;

const ISO3166_ALPHA2 = new Set(
  "AD AE AF AG AI AL AM AO AQ AR AS AT AU AW AX AZ BA BB BD BE BF BG BH BI BJ BL BM BN BO BQ BR BS BT BV BW BY BZ CA CC CD CF CG CH CI CK CL CM CN CO CR CU CV CW CX CY CZ DE DJ DK DM DO DZ EC EE EG EH ER ES ET FI FJ FK FM FO FR GA GB GD GE GF GG GH GI GL GM GN GP GQ GR GS GT GU GW GY HK HM HN HR HT HU ID IE IL IM IN IO IQ IR IS IT JE JM JO JP KE KG KH KI KM KN KP KR KW KY KZ LA LB LC LI LK LR LS LT LU LV LY MA MC MD ME MF MG MH MK ML MM MN MO MP MQ MR MS MT MU MV MW MX MY MZ NA NC NE NF NG NI NL NO NP NR NU NZ OM PA PE PF PG PH PK PL PM PN PR PS PT PW PY QA RE RO RS RU RW SA SB SC SD SE SG SH SI SJ SK SL SM SN SO SR SS ST SV SX SY SZ TC TD TF TG TH TJ TK TL TM TN TO TR TT TV TW TZ UA UG UM US UY UZ VA VC VE VG VI VN VU WF WS YE YT ZA ZM ZW".split(
    " ",
  ),
);

export class AccountBridgeError extends Error {
  constructor(public readonly code: string) {
    super(code);
    this.name = "AccountBridgeError";
  }
}

function isOptionalUsageAvailabilityError(error: unknown): boolean {
  if (!(error instanceof AccountBridgeError)) return false;
  if (error.code === "facade-unavailable") return true;
  const match = /^facade-http-(\d{3})(?:-|$)/.exec(error.code);
  if (!match) return false;
  const status = Number(match[1]);
  // The usage facade is an optional observation only when it is genuinely
  // unavailable (timeout/server failure). 429 may itself be the upstream
  // quota/cooldown signal and 425 is an explicit client-visible refusal, so
  // neither may be converted into a permissive "unknown" snapshot.
  return status === 408 || status >= 500;
}

export interface SignedEligibilityGrant {
  payload: string;
  signature: string;
}

export interface SignedConnectorPolicyDirectory {
  payload: string;
  signature: string;
}

export interface AccountBridgeControlPlaneRequest {
  requestNonce: string;
  crabCodeRelease: string;
  accountBridgeComponentVersion: string;
  accountBridgeProtocolVersion: number;
}

interface InclusiveVersionRange {
  minimumInclusive: string;
  maximumInclusive: string;
}

interface InclusiveProtocolRange {
  minimumInclusive: number;
  maximumInclusive: number;
}

interface AccountBridgeAllowedClientVersions {
  crabCodeRelease: InclusiveVersionRange;
  accountBridgeComponentVersion: InclusiveVersionRange;
  accountBridgeProtocolVersion: InclusiveProtocolRange;
}

export interface AccountBridgeConnectorPolicy {
  connectorId: string;
  displayName: string;
  authMode: (typeof CONNECTOR_AUTH_MODES)[number];
  featureEnabled: boolean;
  termsStatus: "signed-off" | "blocked";
  conformancePassed: boolean;
  fixedArtifactVerified: boolean;
  /**
   * Region class of this connector. Signed field on v2 directory entries;
   * for v1 directories (which predate the field) it is filled from the
   * client-baked `CONNECTOR_REGION_POLICY` map so the type stays total and
   * the sidecar bootstrap always carries it.
   */
  regionPolicy: ConnectorRegionPolicy;
}

export interface AccountBridgeControlPlaneEnvelope {
  eligibilityGrant: SignedEligibilityGrant;
  connectorDirectory: SignedConnectorPolicyDirectory;
}

interface EligibilityPayload {
  audience: string;
  version: string;
  client: AccountBridgeControlPlaneRequest;
  allowedClientVersions: AccountBridgeAllowedClientVersions;
  policyVersion: string;
  issuedAt: number;
  expiresAt: number;
  countryCode: string;
  regionAllowed: boolean;
  connectors: string[];
}

interface VerifiedEligibility {
  grant: SignedEligibilityGrant;
  payload: EligibilityPayload;
  view: AccountBridgeEligibilityView;
  /** Verified control-plane generation this grant was issued under. */
  generation: ControlPlaneGeneration;
}

interface ConnectorPolicyDirectoryPayload {
  audience: string;
  version: string;
  client: AccountBridgeControlPlaneRequest;
  allowedClientVersions: AccountBridgeAllowedClientVersions;
  policyVersion: string;
  issuedAt: number;
  expiresAt: number;
  connectors: AccountBridgeConnectorPolicy[];
}

interface VerifiedConnectorPolicyDirectory {
  signed: SignedConnectorPolicyDirectory;
  payload: ConnectorPolicyDirectoryPayload;
  policies: AccountBridgeConnectorPolicy[];
  /** Verified control-plane generation this directory was issued under. */
  generation: ControlPlaneGeneration;
}

export interface AccountBridgeArtifact {
  binaryPath: string;
  metadataDir: string;
  componentVersion: string;
  protocolVersion: number;
  fixedPlugins: Array<{ id: "gemini-cli"; path: string; sha256: string }>;
}

export interface AccountBridgeSidecarLogLine {
  stream: "stdout" | "stderr";
  timestamp: string | null;
  level: string | null;
  origin: string | null;
  message: string | null;
  redactedByteLength: number | null;
}

export interface AccountBridgeSidecarForensics {
  exitCode: number | null;
  exitSignal: string | null;
  spawnErrorCode: string | null;
  bannerLine: string | null;
  logLines: AccountBridgeSidecarLogLine[];
  droppedLineCount: number;
  readyLineRejected: boolean;
}

export interface AccountBridgeChild {
  readonly pid: number;
  readonly endpoint: Promise<string>;
  readonly exited: Promise<number | null>;
  terminate(): void;
  kill(): void;
  forensics(): AccountBridgeSidecarForensics;
}

export interface AccountBridgeSpawnInput {
  binaryPath: string;
  configPath: string;
  bootstrap: Uint8Array;
  env: NodeJS.ProcessEnv;
  platform: NodeJS.Platform;
  expectedProtocolVersion: number;
}

interface RuntimeLock {
  release(): Promise<void>;
}

/**
 * Persisted user authorization for the region-detection request
 * (`userSettings.accountBridge.eligibilityConsent`, written only by the
 * `setAccountBridgeEligibilityConsent` config edit). `null` = withdrawn or
 * never granted.
 */
export interface AccountBridgeEligibilityConsent {
  grantedAtIso: string;
  noticeVersion: number;
}

/**
 * Privacy-notice version a persisted consent must have been granted against.
 * Kept in lockstep with `config-write.ts::ACCOUNT_BRIDGE_CONSENT_NOTICE_VERSION`
 * (the writer). A stored grant with an older noticeVersion is treated as not
 * granted (fail-closed) so a future notice revision re-prompts the user.
 */
const REQUIRED_CONSENT_NOTICE_VERSION = 1;

export interface AccountBridgeManagerDeps {
  now(): number;
  sleep(ms: number): Promise<void>;
  randomBytes(size: number): Uint8Array;
  /**
   * Read the persisted region-detection consent. The manager is the single
   * enforcement point: `eligibilityRead` and every control-plane refresh
   * fail closed (reasonCode `consent-required`) while this returns `null`
   * or a grant older than {@link REQUIRED_CONSENT_NOTICE_VERSION}. The
   * production default resolves the settings truth source lazily (dynamic
   * import in the source-free bundle), hence the Promise-compatible shape;
   * synchronous test stubs satisfy it unchanged.
   */
  readEligibilityConsent():
    | AccountBridgeEligibilityConsent
    | null
    | Promise<AccountBridgeEligibilityConsent | null>;
  releaseIdentity(): Omit<AccountBridgeControlPlaneRequest, "requestNonce">;
  fetchControlPlane(
    request: AccountBridgeControlPlaneRequest,
  ): Promise<AccountBridgeControlPlaneEnvelope>;
  verifyGrant(
    grant: SignedEligibilityGrant,
    request: AccountBridgeControlPlaneRequest,
    now: number,
  ): VerifiedEligibility;
  verifyConnectorDirectory(
    directory: SignedConnectorPolicyDirectory,
    request: AccountBridgeControlPlaneRequest,
    now: number,
  ): VerifiedConnectorPolicyDirectory;
  resolveAndVerifyArtifact(): Promise<AccountBridgeArtifact>;
  loadOrCreateMasterKey(): Promise<Uint8Array>;
  acquireRuntimeLock(): Promise<RuntimeLock | null>;
  createRuntimeFiles(input: {
    managementKey: string;
    inferenceKey: string;
    fixedPlugins: AccountBridgeArtifact["fixedPlugins"];
  }): Promise<{ configPath: string; runtimeDir: string }>;
  removeRuntimeFiles(runtimeDir: string): Promise<void>;
  spawnSidecar(input: AccountBridgeSpawnInput): Promise<AccountBridgeChild>;
  http(input: {
    url: string;
    method: "GET" | "POST" | "DELETE";
    bearer: string | null;
    body?: unknown;
  }): Promise<unknown>;
  writeDiagnosticBundle(
    bundle: AccountBridgeLocalDiagnosticBundle,
  ): Promise<void>;
  platform: NodeJS.Platform;
  subscribeNetworkChanges?(listener: () => void): () => void;
}

export interface AccountBridgeLocalDiagnosticBundle {
  schemaVersion: 1;
  generatedAt: string;
  runtime: {
    state: AccountBridgeRuntimeView["state"];
    componentVersion: string | null;
    protocolVersion: number | null;
    lastErrorCode: string | null;
  };
  eligibility: {
    state: AccountBridgeEligibilityView["state"];
    policyVersion: string | null;
    checkedAt: string | null;
    expiresAt: string | null;
    reasonCode: string | null;
  } | null;
  sidecar: {
    connectors: Array<{
      connectorId: string;
      enabled: boolean;
      disabledReasonCode: string | null;
      termsStatus: AccountBridgeConnectorView["termsStatus"];
    }>;
    accounts: Array<{
      accountRef: string;
      connectorId: string;
      status: AccountBridgeAccountView["status"];
      connectedAt: string | null;
      lastUsedAt: string | null;
      cooldownUntil: string | null;
    }>;
    routes: Array<{
      routeRef: string;
      accountRef: string;
      connectorId: string;
      modelRef: string;
      chatRuntimeSupported: boolean | null;
      supportsTools: boolean | null;
      supportsThinking: boolean | null;
      supportsAdaptiveThinking: boolean | null;
      supportsEffort: boolean | null;
      supportsMaxEffort: boolean | null;
      supportsVision: boolean | null;
      supportsJsonMode: boolean | null;
      supportedThinkingModes: readonly string[];
      defaultThinkingMode: string | null;
      contextWindow: number | null;
      maxOutputTokens: number | null;
    }>;
    usage: Array<{
      routeRef: string;
      accountRef: string;
      state: AccountBridgeUsageSnapshot["state"];
      remainingPercent: number | null;
      limitingWindowLabel: string | null;
      resetsAt: string | null;
      windows: Array<{
        label: string;
        limit: number | null;
        used: number | null;
        remainingPercent: number | null;
        resetsAt: string | null;
      }>;
      observedAt: string | null;
    }>;
  } | null;
  collectionErrorCode: "sidecar-snapshot-unavailable" | null;
  sidecarProcess: AccountBridgeSidecarForensics | null;
  sidecarProcessHistory: Array<{
    capturedAt: string;
    readyDurationMs: number | null;
    forensics: AccountBridgeSidecarForensics | null;
  }>;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

export function parseAccountBridgeControlPlaneEnvelope(
  value: unknown,
): AccountBridgeControlPlaneEnvelope {
  if (
    !isRecord(value) ||
    JSON.stringify(Object.keys(value).sort()) !==
      JSON.stringify(["connectorDirectory", "eligibilityGrant"])
  ) {
    throw new AccountBridgeError("eligibility-response-invalid");
  }
  for (const field of ["eligibilityGrant", "connectorDirectory"] as const) {
    const signed = value[field];
    if (
      !isRecord(signed) ||
      JSON.stringify(Object.keys(signed).sort()) !==
        JSON.stringify(["payload", "signature"]) ||
      typeof signed.payload !== "string" ||
      typeof signed.signature !== "string"
    ) {
      throw new AccountBridgeError("eligibility-response-invalid");
    }
  }
  return value as unknown as AccountBridgeControlPlaneEnvelope;
}

function strictBase64URL(value: unknown, expectedBytes?: number): Uint8Array {
  if (typeof value !== "string" || !/^[A-Za-z0-9_-]+$/.test(value)) {
    throw new AccountBridgeError("invalid-base64url");
  }
  const bytes = Buffer.from(value, "base64url");
  if (
    bytes.length === 0 ||
    Buffer.from(bytes).toString("base64url") !== value ||
    (expectedBytes !== undefined && bytes.length !== expectedBytes)
  ) {
    bytes.fill(0);
    throw new AccountBridgeError("invalid-base64url");
  }
  return bytes;
}

const RELEASE_VERSION_PATTERN = /^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?$/;

function isCanonicalReleaseVersion(value: unknown): value is string {
  return (
    typeof value === "string" &&
    RELEASE_VERSION_PATTERN.test(value) &&
    validSemver(value) === value
  );
}

function exactControlPlaneClient(
  value: unknown,
  malformedCode: string,
): AccountBridgeControlPlaneRequest {
  if (
    !isRecord(value) ||
    JSON.stringify(Object.keys(value).sort()) !==
      JSON.stringify(
        [
          "accountBridgeComponentVersion",
          "accountBridgeProtocolVersion",
          "crabCodeRelease",
          "requestNonce",
        ].sort(),
      ) ||
    typeof value.requestNonce !== "string" ||
    !/^[A-Za-z0-9_-]{43}$/.test(value.requestNonce) ||
    !isCanonicalReleaseVersion(value.crabCodeRelease) ||
    !isCanonicalReleaseVersion(value.accountBridgeComponentVersion) ||
    !Number.isSafeInteger(value.accountBridgeProtocolVersion) ||
    Number(value.accountBridgeProtocolVersion) <= 0
  ) {
    throw new AccountBridgeError(malformedCode);
  }
  const nonce = strictBase64URL(value.requestNonce, 32);
  nonce.fill(0);
  return {
    requestNonce: value.requestNonce,
    crabCodeRelease: value.crabCodeRelease,
    accountBridgeComponentVersion: value.accountBridgeComponentVersion,
    accountBridgeProtocolVersion: Number(value.accountBridgeProtocolVersion),
  };
}

function exactAllowedClientVersions(
  value: unknown,
  malformedCode: string,
): AccountBridgeAllowedClientVersions {
  if (
    !isRecord(value) ||
    JSON.stringify(Object.keys(value).sort()) !==
      JSON.stringify(
        [
          "accountBridgeComponentVersion",
          "accountBridgeProtocolVersion",
          "crabCodeRelease",
        ].sort(),
      )
  ) {
    throw new AccountBridgeError(malformedCode);
  }
  const parseSemverRange = (candidate: unknown): InclusiveVersionRange => {
    if (
      !isRecord(candidate) ||
      JSON.stringify(Object.keys(candidate).sort()) !==
        JSON.stringify(["maximumInclusive", "minimumInclusive"]) ||
      !isCanonicalReleaseVersion(candidate.minimumInclusive) ||
      !isCanonicalReleaseVersion(candidate.maximumInclusive) ||
      compareSemver(candidate.minimumInclusive, candidate.maximumInclusive) > 0
    ) {
      throw new AccountBridgeError(malformedCode);
    }
    return candidate as unknown as InclusiveVersionRange;
  };
  const protocol = value.accountBridgeProtocolVersion;
  if (
    !isRecord(protocol) ||
    JSON.stringify(Object.keys(protocol).sort()) !==
      JSON.stringify(["maximumInclusive", "minimumInclusive"]) ||
    !Number.isSafeInteger(protocol.minimumInclusive) ||
    !Number.isSafeInteger(protocol.maximumInclusive) ||
    Number(protocol.minimumInclusive) <= 0 ||
    Number(protocol.maximumInclusive) < Number(protocol.minimumInclusive)
  ) {
    throw new AccountBridgeError(malformedCode);
  }
  return {
    crabCodeRelease: parseSemverRange(value.crabCodeRelease),
    accountBridgeComponentVersion: parseSemverRange(
      value.accountBridgeComponentVersion,
    ),
    accountBridgeProtocolVersion:
      protocol as unknown as InclusiveProtocolRange,
  };
}

function verifyControlPlaneClientBinding(
  clientValue: unknown,
  allowedValue: unknown,
  requestValue: AccountBridgeControlPlaneRequest,
  malformedCode: string,
): {
  client: AccountBridgeControlPlaneRequest;
  allowedClientVersions: AccountBridgeAllowedClientVersions;
} {
  const request = exactControlPlaneClient(requestValue, "control-plane-request-invalid");
  const client = exactControlPlaneClient(clientValue, malformedCode);
  const allowedClientVersions = exactAllowedClientVersions(
    allowedValue,
    malformedCode,
  );
  if (client.requestNonce !== request.requestNonce) {
    throw new AccountBridgeError("control-plane-response-replay");
  }
  if (
    client.crabCodeRelease !== request.crabCodeRelease ||
    client.accountBridgeComponentVersion !==
      request.accountBridgeComponentVersion ||
    client.accountBridgeProtocolVersion !==
      request.accountBridgeProtocolVersion
  ) {
    throw new AccountBridgeError("control-plane-response-version-mismatch");
  }
  const semverAllowed = (
    version: string,
    range: InclusiveVersionRange,
  ): boolean =>
    compareSemver(version, range.minimumInclusive) >= 0 &&
    compareSemver(version, range.maximumInclusive) <= 0;
  if (
    !semverAllowed(
      request.crabCodeRelease,
      allowedClientVersions.crabCodeRelease,
    ) ||
    !semverAllowed(
      request.accountBridgeComponentVersion,
      allowedClientVersions.accountBridgeComponentVersion,
    ) ||
    request.accountBridgeProtocolVersion <
      allowedClientVersions.accountBridgeProtocolVersion.minimumInclusive ||
    request.accountBridgeProtocolVersion >
      allowedClientVersions.accountBridgeProtocolVersion.maximumInclusive
  ) {
    throw new AccountBridgeError("control-plane-client-version-unsupported");
  }
  return { client, allowedClientVersions };
}

function exactPayload(
  value: unknown,
  request: AccountBridgeControlPlaneRequest,
): EligibilityPayload {
  if (!isRecord(value)) throw new AccountBridgeError("malformed-grant");
  const expected = [
    "audience",
    "version",
    "client",
    "allowedClientVersions",
    "policyVersion",
    "issuedAt",
    "expiresAt",
    "countryCode",
    "regionAllowed",
    "connectors",
  ].sort();
  if (JSON.stringify(Object.keys(value).sort()) !== JSON.stringify(expected)) {
    throw new AccountBridgeError("malformed-grant");
  }
  const binding = verifyControlPlaneClientBinding(
    value.client,
    value.allowedClientVersions,
    request,
    "malformed-grant",
  );
  if (
    typeof value.audience !== "string" ||
    typeof value.version !== "string" ||
    typeof value.policyVersion !== "string" ||
    value.policyVersion.length === 0 ||
    !Number.isSafeInteger(value.issuedAt) ||
    !Number.isSafeInteger(value.expiresAt) ||
    typeof value.countryCode !== "string" ||
    typeof value.regionAllowed !== "boolean" ||
    !Array.isArray(value.connectors)
  ) {
    throw new AccountBridgeError("malformed-grant");
  }
  const connectors = value.connectors;
  if (
    connectors.length === 0 ||
    connectors.length > 64 ||
    connectors.some(
      connector =>
        typeof connector !== "string" ||
        !/^[a-z0-9][a-z0-9._-]{0,63}$/.test(connector),
    ) ||
    new Set(connectors).size !== connectors.length
  ) {
    throw new AccountBridgeError("invalid-connectors");
  }
  return {
    ...(value as unknown as EligibilityPayload),
    ...binding,
  };
}

function isoTime(unixSeconds: number): string {
  return new Date(unixSeconds * 1_000).toISOString();
}

export function verifyEligibilityGrantWithKey(
  grant: SignedEligibilityGrant,
  publicKeyBase64URL: string,
  request: AccountBridgeControlPlaneRequest,
  now: number,
): VerifiedEligibility {
  if (!isRecord(grant) || Object.keys(grant).length !== 2) {
    throw new AccountBridgeError("malformed-grant");
  }
  const payloadBytes = strictBase64URL(grant.payload);
  const signatureBytes = strictBase64URL(grant.signature, 64);
  const rawPublicKey = strictBase64URL(publicKeyBase64URL, 32);
  try {
    const spkiPrefix = Buffer.from("302a300506032b6570032100", "hex");
    const publicKey = createPublicKey({
      key: Buffer.concat([spkiPrefix, rawPublicKey]),
      format: "der",
      type: "spki",
    });
    if (!verifySignature(null, payloadBytes, publicKey, signatureBytes)) {
      throw new AccountBridgeError("invalid-signature");
    }
    let decoded: unknown;
    try {
      decoded = JSON.parse(Buffer.from(payloadBytes).toString("utf8"));
    } catch {
      throw new AccountBridgeError("malformed-grant");
    }
    const payload = exactPayload(decoded, request);
    const nowSeconds = Math.floor(now / 1_000);
    const countryCode = payload.countryCode.trim().toUpperCase();
    // Same exact 10-key payload for both generations; only the pinned
    // version literal differs. The aggregate state derivation below stays
    // identical — v2 additionally narrows per connector via the signed
    // directory's regionPolicy (see allowedConnectorIds).
    if (
      payload.audience !== GRANT_AUDIENCE ||
      (payload.version !== GRANT_VERSION &&
        payload.version !== GRANT_VERSION_V2) ||
      payload.issuedAt <= 0 ||
      payload.issuedAt > nowSeconds ||
      payload.expiresAt <= nowSeconds ||
      payload.expiresAt <= payload.issuedAt ||
      (payload.expiresAt - payload.issuedAt) * 1_000 > GRANT_MAX_TTL_MS
    ) {
      throw new AccountBridgeError("invalid-grant-policy");
    }
    if (!ISO3166_ALPHA2.has(countryCode)) {
      throw new AccountBridgeError("unknown-region");
    }
    const state =
      countryCode === "CN"
        ? "blocked-cn"
        : payload.regionAllowed
          ? "allowed"
          : "unavailable";
    return {
      grant,
      generation: payload.version as ControlPlaneGeneration,
      payload: { ...payload, countryCode },
      view: parseAccountBridgeEligibilityView({
        state,
        countryCode,
        policyVersion: payload.policyVersion,
        checkedAt: isoTime(payload.issuedAt),
        expiresAt: isoTime(payload.expiresAt),
        reasonCode:
          state === "allowed"
            ? null
            : state === "blocked-cn"
              ? "blocked-cn"
              : "region-not-allowed",
      }),
    };
  } finally {
    payloadBytes.fill(0);
    signatureBytes.fill(0);
    rawPublicKey.fill(0);
  }
}

export function verifyConnectorPolicyDirectoryWithKey(
  directory: SignedConnectorPolicyDirectory,
  publicKeyBase64URL: string,
  request: AccountBridgeControlPlaneRequest,
  now: number,
): VerifiedConnectorPolicyDirectory {
  if (
    !isRecord(directory) ||
    JSON.stringify(Object.keys(directory).sort()) !==
      JSON.stringify(["payload", "signature"])
  ) {
    throw new AccountBridgeError("connector-policy-directory-malformed");
  }
  const payloadBytes = strictBase64URL(directory.payload);
  const signatureBytes = strictBase64URL(directory.signature, 64);
  const rawPublicKey = strictBase64URL(publicKeyBase64URL, 32);
  try {
    const publicKey = createPublicKey({
      key: Buffer.concat([
        Buffer.from("302a300506032b6570032100", "hex"),
        rawPublicKey,
      ]),
      format: "der",
      type: "spki",
    });
    if (!verifySignature(null, payloadBytes, publicKey, signatureBytes)) {
      throw new AccountBridgeError("connector-policy-signature-invalid");
    }
    let decoded: unknown;
    try {
      decoded = JSON.parse(Buffer.from(payloadBytes).toString("utf8"));
    } catch {
      throw new AccountBridgeError("connector-policy-directory-malformed");
    }
    if (
      !isRecord(decoded) ||
      JSON.stringify(Object.keys(decoded).sort()) !==
        JSON.stringify(
          [
            "audience",
            "allowedClientVersions",
            "client",
            "connectors",
            "expiresAt",
            "issuedAt",
            "policyVersion",
            "version",
          ].sort(),
        ) ||
      decoded.audience !== CONNECTOR_DIRECTORY_AUDIENCE ||
      (decoded.version !== GRANT_VERSION &&
        decoded.version !== GRANT_VERSION_V2) ||
      typeof decoded.policyVersion !== "string" ||
      decoded.policyVersion.length === 0 ||
      !Number.isSafeInteger(decoded.issuedAt) ||
      !Number.isSafeInteger(decoded.expiresAt) ||
      !Array.isArray(decoded.connectors)
    ) {
      throw new AccountBridgeError("connector-policy-directory-malformed");
    }
    const generation = decoded.version as ControlPlaneGeneration;
    const binding = verifyControlPlaneClientBinding(
      decoded.client,
      decoded.allowedClientVersions,
      request,
      "connector-policy-directory-malformed",
    );
    const nowSeconds = Math.floor(now / 1_000);
    if (
      Number(decoded.issuedAt) <= 0 ||
      Number(decoded.issuedAt) > nowSeconds ||
      Number(decoded.expiresAt) <= nowSeconds ||
      Number(decoded.expiresAt) <= Number(decoded.issuedAt) ||
      (Number(decoded.expiresAt) - Number(decoded.issuedAt)) * 1_000 >
        GRANT_MAX_TTL_MS
    ) {
      throw new AccountBridgeError("connector-policy-directory-expired");
    }
    // Generation-exact entry contract: v1 keeps today's 7-key entry over the
    // legacy four connectors; v2 adds the signed `regionPolicy` key (8-key
    // exact) and covers all seven shipped connectors.
    const generationConnectorIds: readonly string[] =
      generation === GRANT_VERSION_V2 ? CONNECTOR_IDS : LEGACY_CONNECTOR_IDS;
    const expectedEntryKeys = JSON.stringify(
      [
        "authMode",
        "conformancePassed",
        "connectorId",
        "displayName",
        "featureEnabled",
        "fixedArtifactVerified",
        ...(generation === GRANT_VERSION_V2 ? ["regionPolicy"] : []),
        "termsStatus",
      ].sort(),
    );
    const policies = decoded.connectors.map(value => {
      if (
        !isRecord(value) ||
        JSON.stringify(Object.keys(value).sort()) !== expectedEntryKeys ||
        typeof value.connectorId !== "string" ||
        !generationConnectorIds.includes(value.connectorId) ||
        typeof value.displayName !== "string" ||
        value.displayName !== value.displayName.trim() ||
        [...value.displayName].length === 0 ||
        [...value.displayName].length > CONNECTOR_DISPLAY_NAME_MAX_CODE_POINTS ||
        CONNECTOR_DISPLAY_NAME_FORBIDDEN.test(value.displayName) ||
        !CONNECTOR_AUTH_MODES.includes(
          value.authMode as (typeof CONNECTOR_AUTH_MODES)[number],
        ) ||
        typeof value.featureEnabled !== "boolean" ||
        (value.termsStatus !== "signed-off" && value.termsStatus !== "blocked") ||
        typeof value.conformancePassed !== "boolean" ||
        typeof value.fixedArtifactVerified !== "boolean" ||
        (generation === GRANT_VERSION_V2 &&
          value.regionPolicy !== "global" &&
          value.regionPolicy !== "non-cn")
      ) {
        throw new AccountBridgeError("connector-policy-directory-malformed");
      }
      return {
        connectorId: value.connectorId,
        displayName: value.displayName,
        authMode: value.authMode as AccountBridgeConnectorPolicy["authMode"],
        featureEnabled: value.featureEnabled,
        termsStatus: value.termsStatus,
        conformancePassed: value.conformancePassed,
        fixedArtifactVerified: value.fixedArtifactVerified,
        regionPolicy:
          generation === GRANT_VERSION_V2
            ? (value.regionPolicy as ConnectorRegionPolicy)
            : CONNECTOR_REGION_POLICY[
                value.connectorId as (typeof CONNECTOR_IDS)[number]
              ],
      } satisfies AccountBridgeConnectorPolicy;
    });
    if (
      policies.length !== generationConnectorIds.length ||
      new Set(policies.map(policy => policy.connectorId)).size !==
        generationConnectorIds.length
    ) {
      throw new AccountBridgeError("connector-policy-directory-incomplete");
    }
    return {
      signed: directory,
      generation,
      payload: {
        ...(decoded as unknown as ConnectorPolicyDirectoryPayload),
        ...binding,
      },
      policies,
    };
  } finally {
    payloadBytes.fill(0);
    signatureBytes.fill(0);
    rawPublicKey.fill(0);
  }
}

function unavailableEligibility(reasonCode: string): AccountBridgeEligibilityView {
  return parseAccountBridgeEligibilityView({
    state: "unavailable",
    countryCode: null,
    policyVersion: null,
    checkedAt: null,
    expiresAt: null,
    reasonCode,
  });
}

/**
 * USER-OVERRIDE-2026-07-16 (W-ACCOUNT-BRIDGE-FULL-ENABLE): the control-plane
 * process flags (featureEnabled / termsStatus / conformancePassed /
 * fixedArtifactVerified) no longer disable shipped connectors — the capability
 * ships fully enabled. Two real gates remain and are unchanged:
 *
 * 1. Region: the signed eligibility grant (CN blocked, unknown fail-closed),
 *    re-verified independently by the sidecar on every bootstrap and turn.
 * 2. Supply chain: `verifyPackagedArtifact` still fail-closes start/login on
 *    any binary/plugin hash, license, provenance or signature mismatch; the
 *    Google connector is additionally narrowed by the locally verified fixed
 *    plugin evidence and can only be narrowed, never widened.
 *
 * The signed directory remains the naming/authMode source when reachable so
 * blocked-region UI still shows real connector rows.
 *
 * W-ACCOUNT-BRIDGE-REGION-CONNECTORS: `regionPolicy` passes through
 * unchanged (the spread) — the Go sidecar requires it on every bootstrap
 * policy and re-enforces the region class per connector. qwen/kimi/zai have
 * no fixed plugin, so like openai/anthropic/xai they take the
 * `fixedArtifactVerified: true` branch; only Google stays narrowed by the
 * locally verified fixed-plugin evidence.
 */
function effectiveConnectorPolicy(
  policy: AccountBridgeConnectorPolicy,
  googleFixedPluginVerified: boolean,
): AccountBridgeConnectorPolicy {
  return {
    ...policy,
    featureEnabled: true,
    termsStatus: "signed-off",
    conformancePassed: true,
    fixedArtifactVerified:
      policy.connectorId === "google" ? googleFixedPluginVerified : true,
  };
}

/**
 * Compute the set of connectors the current signed eligibility actually
 * authorizes (W-ACCOUNT-BRIDGE-REGION-CONNECTORS). This is THE gating truth
 * every former `view.state !== "allowed"` runtime gate now folds into:
 *
 * * v1 generation — aggregate semantics exactly as before: an `allowed`
 *   grant authorizes the legacy four, anything else authorizes nothing.
 * * v2 generation — per-connector: an id is allowed iff the signed grant
 *   lists it AND (for 'non-cn' connectors) the egress is not CN and the
 *   grant's aggregate `regionAllowed` holds. The CN floor is client
 *   defense-in-depth: a mis-issued v2 grant listing a 'non-cn' connector
 *   under a CN egress is still blocked locally (the sidecar re-enforces
 *   per connector as well).
 */
function allowedConnectorIds(
  eligibility: VerifiedEligibility | null,
  policies: ReadonlyArray<
    Pick<AccountBridgeConnectorPolicy, "connectorId" | "regionPolicy">
  >,
): ReadonlySet<string> {
  if (!eligibility) return new Set();
  if (eligibility.generation !== GRANT_VERSION_V2) {
    return eligibility.view.state === "allowed"
      ? new Set(LEGACY_CONNECTOR_IDS)
      : new Set();
  }
  const payload = eligibility.payload;
  const allowed = new Set<string>();
  for (const policy of policies) {
    if (!payload.connectors.includes(policy.connectorId)) continue;
    if (policy.regionPolicy === "non-cn") {
      if (payload.countryCode === "CN") continue;
      if (!payload.regionAllowed) continue;
    }
    allowed.add(policy.connectorId);
  }
  return allowed;
}

/**
 * Honest per-row disabled reason: a CN egress disabling a 'non-cn' connector
 * is a region block; every other disabled row keeps the aggregate
 * `eligibility_denied` reason (missing/expired grant, region-not-allowed,
 * or a v2 grant that simply does not list the connector).
 */
function connectorDisabledReasonCode(
  regionPolicy: ConnectorRegionPolicy,
  countryCode: string | null,
): string {
  return countryCode === "CN" && regionPolicy === "non-cn"
    ? "connector_region_blocked"
    : "eligibility_denied";
}

/**
 * Static presentation fallback for the seven shipped connectors, mirrored
 * from the sidecar's own hard allowlist (`internal/accountbridge/runtime.go`).
 * It is used only when the signed directory is unreachable (blocked or
 * unknown region, control-plane outage) so the page never degrades into an
 * opaque "no connectors" error; every action on these rows stays fail-closed
 * until a verified directory and an authorizing grant exist.
 */
const FALLBACK_CONNECTOR_CATALOG: ReadonlyArray<{
  connectorId: (typeof CONNECTOR_IDS)[number];
  displayName: string;
  authMode: (typeof CONNECTOR_AUTH_MODES)[number];
}> = [
  { connectorId: "openai", displayName: "OpenAI", authMode: "browser" },
  { connectorId: "anthropic", displayName: "Anthropic", authMode: "browser" },
  { connectorId: "google", displayName: "Google", authMode: "browser" },
  { connectorId: "xai", displayName: "xAI", authMode: "device-code" },
  { connectorId: "qwen", displayName: "Qwen Code", authMode: "device-code" },
  { connectorId: "kimi", displayName: "Kimi Code", authMode: "device-code" },
  { connectorId: "zai", displayName: "Z Code", authMode: "device-code" },
];

/**
 * Policy source for fallback rows: the client-baked region map. Lets the
 * same `allowedConnectorIds` helper drive fallback enablement when no signed
 * directory is available.
 */
function fallbackConnectorPolicySource(): Array<
  Pick<AccountBridgeConnectorPolicy, "connectorId" | "regionPolicy">
> {
  return FALLBACK_CONNECTOR_CATALOG.map(entry => ({
    connectorId: entry.connectorId,
    regionPolicy: CONNECTOR_REGION_POLICY[entry.connectorId],
  }));
}

function fallbackConnectorView(
  allowedSet: ReadonlySet<string>,
  countryCode: string | null,
): AccountBridgeConnectorView[] {
  return parseAccountBridgeConnectorList({
    connectors: FALLBACK_CONNECTOR_CATALOG.map(entry => {
      const enabled = allowedSet.has(entry.connectorId);
      return {
        connectorId: entry.connectorId,
        displayName: entry.displayName,
        authMode: entry.authMode,
        enabled,
        disabledReasonCode: enabled
          ? null
          : connectorDisabledReasonCode(
              CONNECTOR_REGION_POLICY[entry.connectorId],
              countryCode,
            ),
        termsStatus: "signed-off",
      };
    }),
  });
}

/**
 * Project the already verified connector directory without starting the
 * sidecar. The labels are signed control-plane data, so CN/unknown UI can
 * remain informative while every connector action stays fail-closed.
 *
 * When a connector is allowed this view describes connector policy only; the
 * login path still calls `ensure()` and the ready sidecar independently
 * verifies the bundled artifact before accepting OAuth.
 */
function connectorDirectoryView(
  directory: VerifiedConnectorPolicyDirectory,
  allowedSet: ReadonlySet<string>,
  countryCode: string | null,
): AccountBridgeConnectorView[] {
  return parseAccountBridgeConnectorList({
    connectors: directory.policies.map(policy => {
      const effective = effectiveConnectorPolicy(policy, true);
      const enabled = allowedSet.has(effective.connectorId);
      return {
        connectorId: effective.connectorId,
        displayName: effective.displayName,
        authMode: effective.authMode,
        enabled,
        disabledReasonCode: enabled
          ? null
          : connectorDisabledReasonCode(effective.regionPolicy, countryCode),
        termsStatus: effective.termsStatus,
      };
    }),
  });
}

function randomKey(deps: AccountBridgeManagerDeps): string {
  const bytes = Buffer.from(deps.randomBytes(32));
  try {
    if (bytes.length !== 32) throw new AccountBridgeError("random-key-failed");
    return bytes.toString("base64url");
  } finally {
    bytes.fill(0);
  }
}

function normalizeModelResponse(value: unknown): unknown {
  if (!isRecord(value)) return value;
  const rawRoutes = Array.isArray(value.routes)
    ? value.routes
    : Array.isArray(value.models)
      ? value.models
      : null;
  if (!rawRoutes) return value;
  return {
    routes: rawRoutes.map(route =>
      isRecord(route)
        ? {
            ...route,
            supportsThinking: route.supportsThinking ?? null,
            supportsAdaptiveThinking: route.supportsAdaptiveThinking ?? null,
            supportsEffort: route.supportsEffort ?? null,
            supportsMaxEffort: route.supportsMaxEffort ?? null,
            supportsVision: route.supportsVision ?? null,
            supportsJsonMode: route.supportsJsonMode ?? null,
            supportedThinkingModes: route.supportedThinkingModes ?? [],
            defaultThinkingMode: route.defaultThinkingMode ?? null,
            contextWindow: route.contextWindow ?? null,
            maxOutputTokens: route.maxOutputTokens ?? null,
          }
        : route,
    ),
  };
}

function runtimeView(
  state: AccountBridgeRuntimeView["state"],
  componentVersion: string | null,
  protocolVersion: number | null,
  lastErrorCode: string | null,
): AccountBridgeRuntimeView {
  return parseAccountBridgeRuntimeView({
    state,
    componentVersion,
    protocolVersion,
    lastErrorCode,
  });
}

function diagnosticCode(value: string | null): string | null {
  if (value === null) return null;
  const normalized = value.trim().toLowerCase();
  return /^[a-z0-9][a-z0-9._-]{0,127}$/.test(normalized)
    ? normalized
    : "untrusted-value";
}

function diagnosticTimestamp(value: string | null): string | null {
  if (value === null) return null;
  const timestamp = Date.parse(value);
  return Number.isFinite(timestamp) ? new Date(timestamp).toISOString() : null;
}

function diagnosticOpaqueRef(
  kind: "account" | "route" | "model",
  value: string,
  salt: Uint8Array,
): string {
  return `${kind}-${createHmac("sha256", salt)
    .update(kind)
    .update("\0")
    .update(value)
    .digest("hex")
    .slice(0, 16)}`;
}

function diagnosticRuntimeView(view: AccountBridgeRuntimeView) {
  return {
    state: view.state,
    componentVersion:
      view.componentVersion !== null &&
      /^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?$/.test(view.componentVersion)
        ? view.componentVersion
        : null,
    protocolVersion: view.protocolVersion,
    lastErrorCode: diagnosticCode(view.lastErrorCode),
  };
}

function buildAccountBridgeLocalDiagnosticBundle(input: {
  generatedAt: number;
  runtime: AccountBridgeRuntimeView;
  eligibility: AccountBridgeEligibilityView | null;
  connectors: AccountBridgeConnectorView[] | null;
  accounts: AccountBridgeAccountView[] | null;
  routes: AccountBridgeModelRouteView[] | null;
  usage: AccountBridgeUsageSnapshot[] | null;
  collectionError: boolean;
  sidecarProcess: AccountBridgeSidecarForensics | null;
  sidecarProcessHistory: readonly AccountBridgeSidecarForensicsRecord[];
}): AccountBridgeLocalDiagnosticBundle {
  const referenceSalt = randomBytes(32);
  let sidecar: AccountBridgeLocalDiagnosticBundle["sidecar"] = null;
  try {
    sidecar =
      input.connectors && input.accounts && input.routes && input.usage
        ? {
            connectors: input.connectors.map(connector => ({
              connectorId:
                diagnosticCode(connector.connectorId) ?? "untrusted-value",
              enabled: connector.enabled,
              disabledReasonCode: diagnosticCode(connector.disabledReasonCode),
              termsStatus: connector.termsStatus,
            })),
            accounts: input.accounts.map(account => ({
              accountRef: diagnosticOpaqueRef(
                "account",
                account.accountId,
                referenceSalt,
              ),
              connectorId:
                diagnosticCode(account.connectorId) ?? "untrusted-value",
              status: account.status,
              connectedAt: diagnosticTimestamp(account.connectedAt),
              lastUsedAt: diagnosticTimestamp(account.lastUsedAt),
              cooldownUntil: diagnosticTimestamp(account.cooldownUntil),
            })),
            routes: input.routes.map(route => ({
              routeRef: diagnosticOpaqueRef(
                "route",
                route.routeId,
                referenceSalt,
              ),
              accountRef: diagnosticOpaqueRef(
                "account",
                route.accountId,
                referenceSalt,
              ),
              connectorId:
                diagnosticCode(route.connectorId) ?? "untrusted-value",
              modelRef: diagnosticOpaqueRef(
                "model",
                route.modelId,
                referenceSalt,
              ),
              chatRuntimeSupported: route.chatRuntimeSupported,
              supportsTools: route.supportsTools,
              supportsThinking: route.supportsThinking,
              supportsAdaptiveThinking: route.supportsAdaptiveThinking,
              supportsEffort: route.supportsEffort,
              supportsMaxEffort: route.supportsMaxEffort,
              supportsVision: route.supportsVision,
              supportsJsonMode: route.supportsJsonMode,
              supportedThinkingModes: [...route.supportedThinkingModes],
              defaultThinkingMode: route.defaultThinkingMode,
              contextWindow: route.contextWindow,
              maxOutputTokens: route.maxOutputTokens,
            })),
            usage: input.usage.map(snapshot => ({
              routeRef: diagnosticOpaqueRef(
                "route",
                snapshot.routeId,
                referenceSalt,
              ),
              accountRef: diagnosticOpaqueRef(
                "account",
                snapshot.accountId,
                referenceSalt,
              ),
              state: snapshot.state,
              remainingPercent: snapshot.remainingPercent,
              limitingWindowLabel: diagnosticCode(
                snapshot.limitingWindowLabel,
              ),
              resetsAt: diagnosticTimestamp(snapshot.resetsAt),
              windows: snapshot.windows.map(window => ({
                label: diagnosticCode(window.label) ?? "untrusted-value",
                limit: window.limit,
                used: window.used,
                remainingPercent: window.remainingPercent,
                resetsAt: diagnosticTimestamp(window.resetsAt),
              })),
              observedAt: diagnosticTimestamp(snapshot.observedAt),
            })),
          }
        : null;
  } finally {
    referenceSalt.fill(0);
  }
  return {
    schemaVersion: 1,
    generatedAt: new Date(input.generatedAt).toISOString(),
    runtime: diagnosticRuntimeView(input.runtime),
    eligibility: input.eligibility
      ? {
          state: input.eligibility.state,
          policyVersion: diagnosticCode(input.eligibility.policyVersion),
          checkedAt: diagnosticTimestamp(input.eligibility.checkedAt),
          expiresAt: diagnosticTimestamp(input.eligibility.expiresAt),
          reasonCode: diagnosticCode(input.eligibility.reasonCode),
        }
      : null,
    sidecar,
    collectionErrorCode: input.collectionError
      ? "sidecar-snapshot-unavailable"
      : null,
    sidecarProcess: input.sidecarProcess,
    sidecarProcessHistory: input.sidecarProcessHistory.map(record => ({
      capturedAt: record.capturedAt,
      readyDurationMs: record.readyDurationMs,
      forensics: record.forensics,
    })),
  };
}

export function accountBridgeAutomaticRestartDelay(
  crashTimes: readonly number[],
  now: number,
): number | null {
  const recentCount = crashTimes.filter(time => now - time <= CRASH_WINDOW_MS).length;
  return recentCount >= 1 && recentCount <= CRASH_BACKOFF_MS.length
    ? CRASH_BACKOFF_MS[recentCount - 1]
    : null;
}

export const SIDECAR_FORENSICS_HISTORY_CAPACITY = 5;

interface AccountBridgeSidecarForensicsRecord {
  capturedAt: string;
  readyDurationMs: number | null;
  forensics: AccountBridgeSidecarForensics | null;
}

export class AccountBridgeManager {
  private state: AccountBridgeRuntimeView["state"] = "stopped";
  private lastErrorCode: string | null = null;
  private lastSidecarForensics: AccountBridgeSidecarForensics | null = null;
  private sidecarForensicsHistory: AccountBridgeSidecarForensicsRecord[] = [];
  private lastReadyAtMs: number | null = null;
  private artifact: AccountBridgeArtifact | null = null;
  private eligibility: VerifiedEligibility | null = null;
  private connectorDirectory: VerifiedConnectorPolicyDirectory | null = null;
  private eligibilityGeneration = 0;
  private eligibilityRefresh: Promise<VerifiedEligibility | null> | null = null;
  private backgroundRefresh: ReturnType<typeof setTimeout> | null = null;
  private lifecycleTail: Promise<void> = Promise.resolve();
  private issuedControlPlaneNonceSet = new Set<string>();
  private child: AccountBridgeChild | null = null;
  private runtimeLock: RuntimeLock | null = null;
  private runtimeDir: string | null = null;
  private endpoint: string | null = null;
  private managementKey: string | null = null;
  private inferenceKey: string | null = null;
  private crashTimes: number[] = [];
  private restartTimer: ReturnType<typeof setTimeout> | null = null;
  private stopping = false;
  private networkChangeFlight: Promise<void> | null = null;
  private networkChangeUnsubscribe: (() => void) | null = null;
  private processShutdownStarted = false;
  private diagnosticWriteFlight: Promise<void> | null = null;
  private loginSessions = new Map<
    string,
    {
      connectorId: string;
    }
  >();

  constructor(private readonly deps: AccountBridgeManagerDeps = productionDeps()) {
    this.networkChangeUnsubscribe =
      this.deps.subscribeNetworkChanges?.(() => {
        if (this.processShutdownStarted) return;
        void this.invalidateForNetworkChange().catch(() => {});
      }) ?? null;
  }

  status(): AccountBridgeRuntimeView {
    return runtimeView(
      this.state,
      this.artifact?.componentVersion ?? null,
      this.artifact?.protocolVersion ?? null,
      this.lastErrorCode,
    );
  }

  /**
   * Refresh the local Account Bridge support snapshot consumed by CrabCode's
   * existing diagnostics/system-logs bundle. This is intentionally not an RPC
   * method: runtime/status is the trigger, and the public runtime DTO remains
   * the frozen four-field view.
   *
   * All identity-like values are hashed before persistence. The bundle never
   * receives facade labels, OAuth material, raw subjects, bearer keys, signed
   * grants, endpoint/port data, or filesystem paths. Collection and disk
   * failures are fail-soft so diagnostics cannot break runtime/status.
   */
  async refreshLocalDiagnostics(): Promise<void> {
    if (this.diagnosticWriteFlight) return this.diagnosticWriteFlight;
    const flight = this.writeLocalDiagnostics().catch(() => {});
    this.diagnosticWriteFlight = flight.finally(() => {
      this.diagnosticWriteFlight = null;
    });
    return this.diagnosticWriteFlight;
  }

  private recordSidecarForensicsHistory(
    forensics: AccountBridgeSidecarForensics | null,
  ): void {
    const now = this.deps.now();
    this.sidecarForensicsHistory.push({
      capturedAt: new Date(now).toISOString(),
      readyDurationMs:
        this.lastReadyAtMs !== null
          ? Math.max(0, now - this.lastReadyAtMs)
          : null,
      forensics,
    });
    this.lastReadyAtMs = null;
    while (
      this.sidecarForensicsHistory.length >
      SIDECAR_FORENSICS_HISTORY_CAPACITY
    ) {
      this.sidecarForensicsHistory.shift();
    }
  }

  private async writeLocalDiagnostics(): Promise<void> {
    let connectors: AccountBridgeConnectorView[] | null = null;
    let accounts: AccountBridgeAccountView[] | null = null;
    let routes: AccountBridgeModelRouteView[] | null = null;
    let usage: AccountBridgeUsageSnapshot[] | null = null;
    let collectionError = false;
    if (this.state === "ready") {
      try {
        [connectors, accounts, routes, usage] = await Promise.all([
          this.connectorList(),
          this.accountList(),
          this.modelList(),
          // The sidecar has no reliable provider quota-fetch adapter in this
          // release. forceRefresh=true rebuilds its current runtime snapshot;
          // it does not synthesize percentages from usage counters.
          this.usageRead(null, true),
        ]);
      } catch {
        // Never persist provider/OAuth error text. The stable code is enough
        // for a support bundle to show that collection was incomplete.
        collectionError = true;
      }
    }
    const bundle = buildAccountBridgeLocalDiagnosticBundle({
      generatedAt: this.deps.now(),
      runtime: this.status(),
      eligibility: this.eligibility?.view ?? null,
      connectors,
      accounts,
      routes,
      usage,
      collectionError,
      sidecarProcess: this.lastSidecarForensics,
      sidecarProcessHistory: this.sidecarForensicsHistory,
    });
    await this.deps.writeDiagnosticBundle(bundle);
  }

  /**
   * Fail-closed detection-consent gate (W-ACCOUNT-BRIDGE-REGION-CONNECTORS).
   * Region detection is itself a disclosure — the control plane derives the
   * region from the request's public egress IP — so no request may leave the
   * machine before the user grants consent, and a withdrawal must stop
   * surfacing previously fetched results. Enforced at exactly two entries:
   * `eligibilityRead` (read surface) and `refreshEligibilityInternal`
   * (every control-plane request funnels through it).
   */
  private async detectionConsentSatisfied(): Promise<boolean> {
    let consent: AccountBridgeEligibilityConsent | null;
    try {
      consent = await this.deps.readEligibilityConsent();
    } catch {
      consent = null;
    }
    return (
      consent !== null &&
      typeof consent.grantedAtIso === "string" &&
      consent.grantedAtIso.length > 0 &&
      Number.isSafeInteger(consent.noticeVersion) &&
      consent.noticeVersion >= REQUIRED_CONSENT_NOTICE_VERSION
    );
  }

  async eligibilityRead(forceRefresh: boolean): Promise<AccountBridgeEligibilityView> {
    await this.lifecycleTail;
    // Consent gate BEFORE the cache: a withdrawn consent must stop surfacing
    // a still-valid cached grant, and consent-absent reads must not trigger
    // any control-plane fetch.
    if (!(await this.detectionConsentSatisfied())) {
      return unavailableEligibility("consent-required");
    }
    const now = this.deps.now();
    if (
      !forceRefresh &&
      this.eligibility &&
      this.eligibility.payload.expiresAt * 1_000 > now
    ) {
      return this.eligibility.view;
    }
    const verified = await this.refreshEligibility();
    return verified?.view ?? unavailableEligibility(this.lastErrorCode ?? "eligibility-unavailable");
  }

  async ensure(explicitRetry = true): Promise<AccountBridgeRuntimeView> {
    return this.runLifecycleTransition(() => this.ensureInternal(explicitRetry));
  }

  private async ensureInternal(
    explicitRetry: boolean,
  ): Promise<AccountBridgeRuntimeView> {
    if (this.state === "ready") return this.status();
    if (explicitRetry) {
      this.crashTimes = [];
      this.lastErrorCode = null;
      if (this.restartTimer) clearTimeout(this.restartTimer);
      this.restartTimer = null;
    } else if (this.state === "failed" && this.crashTimes.length > 3) {
      throw new AccountBridgeError("runtime-retry-required");
    }
    const eligibility = await this.refreshEligibilityInternal(false);
    // Gate on the per-connector allowed set, not the aggregate state: a CN
    // v2 grant that authorizes 'global' connectors must be able to start the
    // runtime (the sidecar re-enforces per connector), while an empty set is
    // exactly the old aggregate denial.
    if (!eligibility || this.currentAllowedConnectorIds().size === 0) {
      this.state = "blocked";
      throw new AccountBridgeError("eligibility-denied");
    }
    return this.startWithCurrentControlPlane();
  }

  async stop(): Promise<AccountBridgeRuntimeView> {
    return this.runLifecycleTransition(() => this.stopInternal());
  }

  /**
   * Permanently settle the process-owned Account Bridge runtime before the
   * worker acknowledges host shutdown. Unlike public `stop()` this also
   * removes the process-lifetime network subscription and, after escalating
   * to SIGKILL, waits for the child exit witness without declaring a timeout
   * to be success. The Rust host owns the outer RPC liveness ceiling and must
   * retain the worker/root authority when that ceiling expires.
   */
  beginProcessShutdown(): void {
    this.processShutdownStarted = true;
    const unsubscribe = this.networkChangeUnsubscribe;
    if (unsubscribe) {
      try {
        unsubscribe();
      } catch {
        throw new AccountBridgeError(
          "runtime-network-subscription-stop-failed",
        );
      }
      this.networkChangeUnsubscribe = null;
    }
  }

  async shutdownForProcess(): Promise<AccountBridgeRuntimeView> {
    this.beginProcessShutdown();
    return this.runLifecycleTransition(() => this.stopInternal(true));
  }

  private async stopInternal(
    awaitKilledExit = false,
  ): Promise<AccountBridgeRuntimeView> {
    this.stopping = true;
    this.state = "stopping";
    if (this.restartTimer) clearTimeout(this.restartTimer);
    this.restartTimer = null;
    if (this.backgroundRefresh) clearTimeout(this.backgroundRefresh);
    this.backgroundRefresh = null;
    const child = this.child;
    if (child) {
      child.terminate();
      let exited = await Promise.race([
        child.exited.then(() => true),
        this.deps.sleep(STOP_GRACE_MS).then(() => false),
      ]);
      if (!exited) {
        child.kill();
        exited = awaitKilledExit
          ? await child.exited.then(
              () => true,
              () => false,
            )
          : await Promise.race([
              child.exited.then(() => true),
              this.deps.sleep(STOP_GRACE_MS).then(() => false),
            ]);
      }
      if (!exited) {
        this.state = "failed";
        this.lastErrorCode = "runtime-stop-incomplete";
        this.stopping = false;
        throw new AccountBridgeError("runtime-stop-incomplete");
      }
      if (this.child === child) this.child = null;
    }
    await this.cleanupRuntimeFiles();
    await this.releaseRuntimeLock();
    this.clearRuntimeSecrets();
    this.endpoint = null;
    this.loginSessions.clear();
    this.state = "stopped";
    this.lastErrorCode = null;
    this.stopping = false;
    return this.status();
  }

  async connectorList(): Promise<AccountBridgeConnectorView[]> {
    // A ready sidecar remains authoritative for the effective policy after
    // local artifact narrowing (notably the fixed Gemini plugin check).
    if (this.state === "ready" && this.endpoint && this.managementKey) {
      return parseAccountBridgeConnectorList(
        await this.facade("/connectors", "GET"),
      );
    }

    // The signed directory is deliberately readable while the sidecar is
    // stopped/blocked. This is presentation data only: a non-authorized
    // connector row is disabled, and login/turn methods still require a
    // freshly verified ready runtime.
    const directory = this.connectorDirectory;
    const eligibility = this.eligibility;
    const countryCode = eligibility?.payload.countryCode ?? null;
    if (
      !directory ||
      directory.payload.expiresAt * 1_000 <= this.deps.now()
    ) {
      // Never collapse the page into "no connector directory": blocked or
      // unknown regions (where the control plane refuses the directory) must
      // still see the seven shipped connectors with an honest disabled
      // reason. Fallback enablement runs the same allowed-set helper over
      // the client-baked region map.
      return fallbackConnectorView(
        allowedConnectorIds(eligibility, fallbackConnectorPolicySource()),
        countryCode,
      );
    }
    return connectorDirectoryView(
      directory,
      allowedConnectorIds(eligibility, directory.policies),
      countryCode,
    );
  }

  async accountList(): Promise<AccountBridgeAccountView[]> {
    return parseAccountBridgeAccountList(await this.facade("/accounts", "GET"));
  }

  async modelList(): Promise<AccountBridgeModelRouteView[]> {
    return parseAccountBridgeModelList(
      normalizeModelResponse(await this.facade("/routes", "GET")),
    );
  }

  async usageRead(
    routeIds: string[] | null,
    forceRefresh: boolean,
  ): Promise<AccountBridgeUsageSnapshot[]> {
    const query = new URLSearchParams();
    if (routeIds) for (const routeId of routeIds) query.append("routeId", routeId);
    query.set("forceRefresh", String(forceRefresh));
    const raw = await this.facade(`/usage?${query.toString()}`, "GET");
    if (isRecord(raw) && Array.isArray(raw.usage) && !Array.isArray(raw.snapshots)) {
      return parseAccountBridgeUsageRead({ snapshots: raw.usage });
    }
    return parseAccountBridgeUsageRead(raw);
  }

  async loginStart(connectorId: string): Promise<AccountBridgeLoginStartView> {
    const connectors = await this.connectorList();
    const connector = connectors.find(item => item.connectorId === connectorId);
    if (!connector || !connector.enabled) {
      throw new AccountBridgeError("connector-disabled");
    }
    const raw = await this.facade("/login/start", "POST", { connectorId });
    if (
      !isRecord(raw) ||
      raw.status !== "ok" ||
      typeof raw.state !== "string" ||
      raw.state.length === 0 ||
      raw.state.length > 512 ||
      /[\u0000-\u001f\u007f]/.test(raw.state) ||
      typeof raw.url !== "string"
    ) {
      throw new AccountBridgeError("login-start-response-invalid");
    }
    let authorizationURL: URL;
    try {
      authorizationURL = new URL(raw.url);
    } catch {
      throw new AccountBridgeError("login-start-response-invalid");
    }
    if (authorizationURL.protocol !== "https:") {
      throw new AccountBridgeError("login-start-response-invalid");
    }
    if (raw.flow !== undefined && raw.flow !== "device") {
      throw new AccountBridgeError("login-start-response-invalid");
    }
    const authMode = raw.flow === "device" ? "device-code" : "browser";
    if (authMode !== connector.authMode) {
      throw new AccountBridgeError("login-start-response-invalid");
    }
    const isDevice = authMode === "device-code";
    const expiresIn = raw.expires_in;
    if (
      isDevice &&
      (!Number.isSafeInteger(expiresIn) ||
        Number(expiresIn) <= 0 ||
        Number(expiresIn) > 3_600 ||
        (raw.user_code !== undefined &&
          raw.user_code !== null &&
          (typeof raw.user_code !== "string" ||
            raw.user_code.length === 0 ||
            raw.user_code.length > 128 ||
            /[\u0000-\u001f\u007f]/.test(raw.user_code))))
    ) {
      throw new AccountBridgeError("login-start-response-invalid");
    }
    const view = parseAccountBridgeLoginStartView({
      sessionId: raw.state,
      authMode,
      authorizationUrl: isDevice ? null : authorizationURL.toString(),
      userCode:
        isDevice && typeof raw.user_code === "string" ? raw.user_code : null,
      verificationUrl: isDevice ? authorizationURL.toString() : null,
      expiresAt: isDevice
        ? new Date(this.deps.now() + Number(expiresIn) * 1_000).toISOString()
        : null,
    });
    this.loginSessions.set(view.sessionId, {
      connectorId,
    });
    return view;
  }

  async loginPoll(sessionId: string): Promise<AccountBridgeLoginPollView> {
    if (!this.loginSessions.has(sessionId)) {
      return parseAccountBridgeLoginPollView({
        state: "session-lost",
        accountId: null,
        errorCode: null,
      });
    }
    const raw = await this.facade(
      `/login/poll?state=${encodeURIComponent(sessionId)}`,
      "GET",
    );
    let sidecarView: AccountBridgeLoginPollView;
    try {
      sidecarView = parseAccountBridgeLoginPollView(raw);
    } catch {
      throw new AccountBridgeError("login-poll-response-invalid");
    }
    let accountId: string | null = null;
    if (sidecarView.state === "succeeded") {
      const session = this.loginSessions.get(sessionId);
      if (!session) throw new AccountBridgeError("login-session-untracked");
      if (
        sidecarView.accountId === null ||
        !/^[A-Za-z0-9_-]{43}$/.test(sidecarView.accountId) ||
        sidecarView.errorCode !== null
      ) {
        throw new AccountBridgeError("login-account-association-unavailable");
      }
      const account = (await this.accountList()).find(
        candidate => candidate.accountId === sidecarView.accountId,
      );
      if (!account || account.connectorId !== session.connectorId) {
        throw new AccountBridgeError("login-account-association-unavailable");
      }
      accountId = sidecarView.accountId;
      this.loginSessions.delete(sessionId);
    } else {
      if (sidecarView.accountId !== null) {
        throw new AccountBridgeError("login-poll-response-invalid");
      }
      if (["failed", "cancelled", "expired"].includes(sidecarView.state)) {
        this.loginSessions.delete(sessionId);
      }
    }
    return parseAccountBridgeLoginPollView({
      state: sidecarView.state,
      accountId,
      errorCode:
        sidecarView.state === "failed" && sidecarView.errorCode !== null
          ? sidecarView.errorCode
          : null,
    });
  }

  async loginCancel(sessionId: string): Promise<AccountBridgeLoginCancelView> {
    if (!this.loginSessions.has(sessionId)) {
      return parseAccountBridgeLoginCancelView({ cancelled: true });
    }
    const view = parseAccountBridgeLoginCancelView(
      await this.facade(`/login/cancel?state=${encodeURIComponent(sessionId)}`, "DELETE"),
    );
    if (view.cancelled) this.loginSessions.delete(sessionId);
    return view;
  }

  async accountRemove(accountId: string): Promise<AccountBridgeAccountRemoveView> {
    return parseAccountBridgeAccountRemoveView(
      await this.facade(`/accounts/${encodeURIComponent(accountId)}`, "DELETE"),
    );
  }

  /**
   * Revalidate every mandatory fact for one opaque account route against the
   * live sidecar immediately before either persisting it as the default model
   * or minting inference access for a turn.
   *
   * Usage is deliberately asymmetric: an explicit fresh cooldown/exhausted
   * snapshot blocks, while a missing/unknown snapshot (or a transient 5xx /
   * timeout from the optional usage facade) does not. The frozen wire contract
   * treats usage as an observation and renders missing data as `—`; account,
   * route, connector and capability truth remain mandatory and fail closed.
   */
  async validateRouteAccess(routeId: string): Promise<AccountBridgeModelRouteView> {
    return this.runLifecycleTransition(() =>
      this.validateRouteAccessInternal(routeId),
    );
  }

  async turnAccess(routeId: string): Promise<AccountBridgeRuntimeAccess> {
    return this.runLifecycleTransition(() => this.turnAccessInternal(routeId));
  }

  private async validateRouteAccessInternal(
    routeId: string,
  ): Promise<AccountBridgeModelRouteView> {
    if (!/^[A-Za-z0-9_-]{43}$/.test(routeId)) {
      throw new AccountBridgeError("invalid-route-id");
    }
    await this.requireTurnEligibilityInternal();
    if (this.state !== "ready") await this.ensureInternal(false);

    const usageRead = this.usageRead([routeId], true).then(
      snapshots => ({ kind: "ok" as const, snapshots }),
      error => ({ kind: "error" as const, error }),
    );
    const [routes, connectors, accounts, usageResult] = await Promise.all([
      this.modelList(),
      this.connectorList(),
      this.accountList(),
      usageRead,
    ]);
    const route = routes.find(candidate => candidate.routeId === routeId);
    if (!route) throw new AccountBridgeError("unknown-account-route");
    const connector = connectors.find(
      candidate => candidate.connectorId === route.connectorId,
    );
    if (
      !connector ||
      !connector.enabled ||
      route.chatRuntimeSupported !== true ||
      route.supportsTools !== true
    ) {
      throw new AccountBridgeError("route-capability-denied");
    }

    const account = accounts.find(candidate => candidate.accountId === route.accountId);
    if (!account) throw new AccountBridgeError("account-route-unavailable");
    if (account.connectorId !== route.connectorId) {
      throw new AccountBridgeError("account-route-binding-invalid");
    }
    if (account.status !== "ready") {
      throw new AccountBridgeError("account-not-ready");
    }

    if (usageResult.kind === "error") {
      if (!isOptionalUsageAvailabilityError(usageResult.error)) {
        throw new AccountBridgeError("usage-response-invalid");
      }
    } else {
      const snapshots = usageResult.snapshots;
      if (
        snapshots.length > 1 ||
        snapshots.some(candidate => candidate.routeId !== routeId)
      ) {
        throw new AccountBridgeError("usage-route-mismatch");
      }
      const usage = snapshots[0];
      if (usage && usage.accountId !== route.accountId) {
        throw new AccountBridgeError("usage-route-mismatch");
      }
      if (usage?.state === "cooldown") {
        throw new AccountBridgeError("route-cooldown");
      }
      if (usage?.state === "exhausted") {
        throw new AccountBridgeError("route-quota-exhausted");
      }
    }

    if (this.state !== "ready" || !this.endpoint) {
      throw new AccountBridgeError("runtime-unavailable");
    }
    return route;
  }

  private async turnAccessInternal(
    routeId: string,
  ): Promise<AccountBridgeRuntimeAccess> {
    const route = await this.validateRouteAccessInternal(routeId);
    if (!this.endpoint || !this.inferenceKey) {
      throw new AccountBridgeError("runtime-unavailable");
    }
    return parseAccountBridgeRuntimeAccess({
      endpoint: `${this.endpoint}/v1/messages`,
      route,
      inferenceKey: this.inferenceKey,
    });
  }

  private runLifecycleTransition<T>(operation: () => Promise<T>): Promise<T> {
    const result = this.lifecycleTail.then(operation, operation);
    this.lifecycleTail = result.then(
      () => undefined,
      () => undefined,
    );
    return result;
  }

  private nextControlPlaneRequest(): AccountBridgeControlPlaneRequest {
    const nonceBytes = Buffer.from(this.deps.randomBytes(32));
    if (nonceBytes.length !== 32) {
      nonceBytes.fill(0);
      throw new AccountBridgeError("control-plane-request-nonce-unavailable");
    }
    const requestNonce = nonceBytes.toString("base64url");
    nonceBytes.fill(0);
    if (this.issuedControlPlaneNonceSet.has(requestNonce)) {
      throw new AccountBridgeError("control-plane-request-nonce-reused");
    }
    this.issuedControlPlaneNonceSet.add(requestNonce);
    return exactControlPlaneClient(
      { requestNonce, ...this.deps.releaseIdentity() },
      "control-plane-request-invalid",
    );
  }

  private async fetchVerifiedControlPlaneInternal(): Promise<{
    eligibility: VerifiedEligibility;
    connectorDirectory: VerifiedConnectorPolicyDirectory;
  } | null> {
    const generation = this.eligibilityGeneration;
    const request = this.nextControlPlaneRequest();
    const controlPlane = await this.deps.fetchControlPlane(request);
    const verified = this.deps.verifyGrant(
      controlPlane.eligibilityGrant,
      request,
      this.deps.now(),
    );
    const connectorDirectory = this.deps.verifyConnectorDirectory(
      controlPlane.connectorDirectory,
      request,
      this.deps.now(),
    );
    // Generation coherence: one control-plane response must not mix a v2
    // grant with a v1 directory (or vice versa) — the per-connector set
    // would otherwise be computed against the wrong entry contract. Throwing
    // here funnels into the existing unavailable/lastErrorCode path.
    if (verified.generation !== connectorDirectory.generation) {
      throw new AccountBridgeError("control-plane-generation-mismatch");
    }
    if (generation !== this.eligibilityGeneration) return null;
    return { eligibility: verified, connectorDirectory };
  }

  /**
   * Allowed-connector set for runtime gates, computed from the CURRENT
   * verified grant + directory. Without a verified directory the check keeps
   * the conservative v1 aggregate semantics (allowed → legacy set, anything
   * else → empty) — exactly today's behavior for directory-less states.
   */
  private currentAllowedConnectorIds(): ReadonlySet<string> {
    const eligibility = this.eligibility;
    if (!eligibility) return new Set();
    const directory = this.connectorDirectory;
    if (!directory) {
      return eligibility.view.state === "allowed"
        ? new Set(LEGACY_CONNECTOR_IDS)
        : new Set();
    }
    return allowedConnectorIds(eligibility, directory.policies);
  }

  private async refreshEligibility(): Promise<VerifiedEligibility | null> {
    if (this.eligibilityRefresh) return this.eligibilityRefresh;
    this.eligibilityRefresh = this.runLifecycleTransition(() =>
      this.refreshEligibilityInternal(true),
    ).finally(() => {
      this.eligibilityRefresh = null;
    });
    return this.eligibilityRefresh;
  }

  private async refreshEligibilityInternal(
    restartActive: boolean,
    activeOverride?: boolean,
  ): Promise<VerifiedEligibility | null> {
    const wasActive =
      activeOverride ??
      (this.child !== null ||
        this.state === "ready" ||
        this.state === "starting");
    // A policy refresh is a fail-closed lifecycle transition, not an in-place
    // mutation. Retire the sidecar that owns the previous grant/directory
    // before requesting or publishing replacement policy. This also closes
    // the public facade while the control plane is in flight.
    if (wasActive) {
      try {
        await this.stopInternal();
      } catch {
        this.eligibility = null;
        this.connectorDirectory = null;
        throw new AccountBridgeError("runtime-stop-incomplete");
      }
    }
    this.eligibility = null;
    this.connectorDirectory = null;
    // Consent gate: without a current detection consent no control-plane
    // request may leave the machine. The active-sidecar retirement above has
    // already run, so a withdrawal also tears down a running runtime on the
    // next refresh instead of letting it coast on the old grant.
    if (!(await this.detectionConsentSatisfied())) {
      this.lastErrorCode = "consent-required";
      return null;
    }
    let next: {
      eligibility: VerifiedEligibility;
      connectorDirectory: VerifiedConnectorPolicyDirectory;
    } | null;
    try {
      next = await this.fetchVerifiedControlPlaneInternal();
    } catch (error) {
      this.eligibility = null;
      this.connectorDirectory = null;
      this.state = "blocked";
      this.lastErrorCode =
        error instanceof AccountBridgeError
          ? error.code
          : "eligibility-unavailable";
      return null;
    }
    if (!next) return null;
    this.eligibility = next.eligibility;
    this.connectorDirectory = next.connectorDirectory;
    if (
      allowedConnectorIds(next.eligibility, next.connectorDirectory.policies)
        .size === 0
    ) {
      this.state = "blocked";
      this.lastErrorCode =
        next.eligibility.view.reasonCode ?? "eligibility-denied";
      return next.eligibility;
    }

    // Keep a failed runtime's diagnostic code observable: a later successful
    // eligibility refresh must not erase the launch error (W-ACCOUNT-BRIDGE
    // 诊断保真度 P1 — the 5-minute grant refresh used to wipe lastErrorCode
    // before anyone could read it, leaving only failed+null on disk).
    if (this.state !== "failed") {
      this.state = "stopped";
      this.lastErrorCode = null;
    }
    if (wasActive && restartActive) {
      try {
        await this.startWithCurrentControlPlane();
      } catch {
        // startWithCurrentControlPlane already records a stable fail-closed
        // runtime error. Preserve the valid signed eligibility projection.
      }
    }
    return next.eligibility;
  }

  private scheduleBackgroundRefresh(): void {
    if (this.backgroundRefresh) clearTimeout(this.backgroundRefresh);
    if (!this.eligibility || !this.connectorDirectory || this.state !== "ready") return;
    const refreshAt = Math.min(
      this.eligibility.payload.issuedAt * 1_000 + GRANT_BACKGROUND_REFRESH_MS,
      this.connectorDirectory.payload.issuedAt * 1_000 + GRANT_BACKGROUND_REFRESH_MS,
    );
    const delay = refreshAt - this.deps.now();
    // A freshly fetched endpoint response can legitimately carry the same
    // still-valid grant after the four-minute mark. Scheduling it at 0ms would
    // spin until expiry; the next turn owns the synchronous expiry refresh.
    if (delay <= 0) return;
    this.backgroundRefresh = setTimeout(() => {
      this.backgroundRefresh = null;
      void this.refreshEligibility().catch(() => {});
    }, delay);
    this.backgroundRefresh.unref?.();
  }

  private async requireTurnEligibilityInternal(): Promise<VerifiedEligibility> {
    const now = this.deps.now();
    if (
      !this.eligibility ||
      this.eligibility.payload.expiresAt * 1_000 <= now ||
      !this.connectorDirectory ||
      this.connectorDirectory.payload.expiresAt * 1_000 <= now
    ) {
      await this.refreshEligibilityInternal(true);
    }
    if (
      !this.eligibility ||
      this.eligibility.payload.expiresAt * 1_000 <= this.deps.now()
    ) {
      throw new AccountBridgeError("eligibility-denied");
    }
    if (
      !this.connectorDirectory ||
      this.connectorDirectory.payload.expiresAt * 1_000 <= this.deps.now()
    ) {
      // No fresh directory → conservative v1 aggregate semantics first
      // (matches the old state-before-directory precedence), then the
      // directory-expiry throw for the aggregate-allowed case.
      if (this.eligibility.view.state !== "allowed") {
        throw new AccountBridgeError("eligibility-denied");
      }
      throw new AccountBridgeError("connector-policy-directory-unavailable");
    }
    if (
      allowedConnectorIds(this.eligibility, this.connectorDirectory.policies)
        .size === 0
    ) {
      throw new AccountBridgeError("eligibility-denied");
    }
    return this.eligibility;
  }

  private async invalidateForNetworkChange(): Promise<void> {
    this.eligibilityGeneration += 1;
    if (this.backgroundRefresh) clearTimeout(this.backgroundRefresh);
    this.backgroundRefresh = null;
    if (this.networkChangeFlight) return this.networkChangeFlight;
    const restartActive =
      this.child !== null || this.state === "ready" || this.state === "starting";
    this.networkChangeFlight = this.runLifecycleTransition(async () => {
      while (true) {
        const targetGeneration = this.eligibilityGeneration;
        if (this.child !== null) await this.stopInternal();
        this.eligibility = null;
        this.connectorDirectory = null;
        const verified = await this.refreshEligibilityInternal(false, false);
        if (targetGeneration !== this.eligibilityGeneration) continue;
        if (!verified || this.currentAllowedConnectorIds().size === 0) {
          this.state = "blocked";
          this.lastErrorCode ??= "network-change-revalidation-failed";
        } else if (restartActive) {
          try {
            await this.startWithCurrentControlPlane();
          } catch {
            // The start path already records the fail-closed runtime code.
          }
        } else {
          this.state = "stopped";
          this.lastErrorCode = null;
        }
        break;
      }
    }).finally(() => {
      this.networkChangeFlight = null;
    });
    return this.networkChangeFlight;
  }

  private async startWithCurrentControlPlane(): Promise<AccountBridgeRuntimeView> {
    this.state = "starting";
    let spawnedChild: AccountBridgeChild | null = null;
    try {
      const eligibility = this.eligibility;
      if (
        !eligibility ||
        eligibility.payload.expiresAt * 1_000 <= this.deps.now()
      ) {
        this.state = "blocked";
        throw new AccountBridgeError("eligibility-denied");
      }
      const connectorDirectory = this.connectorDirectory;
      if (
        !connectorDirectory ||
        connectorDirectory.payload.expiresAt * 1_000 <= this.deps.now()
      ) {
        // Directory-less states keep the old precedence: an aggregate
        // denial reads as eligibility-denied, only the aggregate-allowed
        // case surfaces the missing directory.
        if (eligibility.view.state !== "allowed") {
          this.state = "blocked";
          throw new AccountBridgeError("eligibility-denied");
        }
        throw new AccountBridgeError("connector-policy-directory-unavailable");
      }
      if (
        allowedConnectorIds(eligibility, connectorDirectory.policies).size ===
        0
      ) {
        this.state = "blocked";
        throw new AccountBridgeError("eligibility-denied");
      }
      this.artifact = await this.deps.resolveAndVerifyArtifact();
      const googleFixedPluginVerified = this.artifact.fixedPlugins.some(
        plugin => plugin.id === "gemini-cli",
      );
      // The bootstrap carries the locally derived effective policy: shipped
      // connectors are enabled (USER-OVERRIDE-2026-07-16, see
      // effectiveConnectorPolicy), while local artifact evidence remains an
      // independent gate for the fixed Google plugin. Narrowing Google to
      // false is safe and keeps the other three connectors operational; never
      // discover from PATH or download a missing plugin.
      const effectiveConnectorPolicies = connectorDirectory.policies.map(policy =>
        effectiveConnectorPolicy(policy, googleFixedPluginVerified),
      );
      if (!this.runtimeLock) {
        this.runtimeLock = await this.deps.acquireRuntimeLock();
        if (!this.runtimeLock) {
          throw new AccountBridgeError("runtime-owned-by-another-process");
        }
      }
      const masterKey = Buffer.from(await this.deps.loadOrCreateMasterKey());
      try {
        if (masterKey.length !== 32) {
          throw new AccountBridgeError("master-key-unavailable");
        }
        const managementKey = randomKey(this.deps);
        const inferenceKey = randomKey(this.deps);
        if (managementKey === inferenceKey) {
          throw new AccountBridgeError("random-key-collision");
        }
        const files = await this.deps.createRuntimeFiles({
          managementKey,
          inferenceKey,
          fixedPlugins: this.artifact.fixedPlugins,
        });
        this.runtimeDir = files.runtimeDir;
        const bootstrap = Buffer.from(
          JSON.stringify({
            masterKey: masterKey.toString("base64url"),
            requestNonce: eligibility.payload.client.requestNonce,
            eligibilityGrant: eligibility.grant,
            connectorPolicies: effectiveConnectorPolicies,
          }),
          "utf8",
        );
        masterKey.fill(0);
        try {
          const child = await this.deps.spawnSidecar({
            binaryPath: this.artifact.binaryPath,
            configPath: files.configPath,
            bootstrap,
            env: sanitizedSidecarEnv(process.env),
            platform: this.deps.platform,
            expectedProtocolVersion: this.artifact.protocolVersion,
          });
          spawnedChild = child;
          this.child = child;
          this.managementKey = managementKey;
          this.inferenceKey = inferenceKey;
          const endpoint = await child.endpoint;
          if (!/^http:\/\/127\.0\.0\.1:[1-9]\d{0,4}$/.test(endpoint)) {
            throw new AccountBridgeError("runtime-endpoint-invalid");
          }
          this.endpoint = endpoint;
          await this.deps.http({
            url: `${endpoint}/healthz`,
            method: "GET",
            bearer: null,
          });
          // The sidecar installs its file watcher shortly after readiness.
          // Keep the config inside the private runtime directory until exit;
          // deleting it here can race watcher setup and crash-loop the child.
          this.state = "ready";
          this.lastErrorCode = null;
          this.lastReadyAtMs = this.deps.now();
          this.scheduleBackgroundRefresh();
          void child.exited
            .then(code =>
              this.runLifecycleTransition(() =>
                this.onUnexpectedExitInternal(child, code),
              ),
            )
            .catch(() => {});
          return this.status();
        } finally {
          bootstrap.fill(0);
        }
      } finally {
        masterKey.fill(0);
      }
    } catch (error) {
      let errorCode =
        error instanceof AccountBridgeError ||
        error instanceof AccountBridgeMasterKeyError
          ? error.code
          : "runtime-start-failed";
      try {
        await this.stopInternal();
      } catch {
        errorCode = "runtime-stop-incomplete";
      }
      this.state =
        errorCode === "eligibility-denied" ||
        errorCode === "master-key-secure-storage-unavailable" ||
        errorCode === "artifact-missing"
          ? "blocked"
          : "failed";
      this.lastErrorCode = errorCode;
      this.lastSidecarForensics = spawnedChild?.forensics() ?? null;
      if (spawnedChild) {
        this.recordSidecarForensicsHistory(spawnedChild.forensics());
      }
      throw new AccountBridgeError(errorCode);
    }
  }

  private async onUnexpectedExitInternal(
    child: AccountBridgeChild,
    _code: number | null,
  ): Promise<void> {
    if (this.child !== child || this.stopping) return;
    this.lastSidecarForensics = child.forensics();
    this.recordSidecarForensicsHistory(child.forensics());
    this.loginSessions.clear();
    this.child = null;
    this.endpoint = null;
    this.clearRuntimeSecrets();
    await this.cleanupRuntimeFiles();
    const now = this.deps.now();
    this.crashTimes = this.crashTimes.filter(time => now - time <= CRASH_WINDOW_MS);
    this.crashTimes.push(now);
    const delay = accountBridgeAutomaticRestartDelay(this.crashTimes, now);
    if (delay === null) {
      this.state = "failed";
      this.lastErrorCode = "runtime-crash-limit";
      return;
    }
    this.state = "starting";
    this.lastErrorCode = "runtime-restarting";
    this.restartTimer = setTimeout(() => {
      this.restartTimer = null;
      if (this.stopping) return;
      void this.runLifecycleTransition(async () => {
        if (this.stopping || this.child !== null || this.state === "ready") return;
        const eligibility = await this.refreshEligibilityInternal(false);
        if (!eligibility) {
          this.state = "failed";
          this.lastErrorCode = "runtime-start-failed";
          return;
        }
        if (this.currentAllowedConnectorIds().size === 0) {
          this.state = "blocked";
          this.lastErrorCode = "eligibility-denied";
          return;
        }
        await this.startWithCurrentControlPlane();
      }).catch(() => {
        if (this.state === "starting") {
          this.state = "failed";
          this.lastErrorCode = "runtime-start-failed";
        }
      });
    }, delay);
  }

  private async facade(
    path: string,
    method: "GET" | "POST" | "DELETE",
    body?: unknown,
  ): Promise<unknown> {
    if (this.state !== "ready" || !this.endpoint || !this.managementKey) {
      throw new AccountBridgeError("runtime-unavailable");
    }
    return this.deps.http({
      url: `${this.endpoint}${FACADE_PATH}${path}`,
      method,
      bearer: this.managementKey,
      ...(body === undefined ? {} : { body }),
    });
  }

  private clearRuntimeSecrets(): void {
    this.managementKey = null;
    this.inferenceKey = null;
  }

  private async cleanupRuntimeFiles(): Promise<void> {
    const runtimeDir = this.runtimeDir;
    this.runtimeDir = null;
    if (runtimeDir) await this.deps.removeRuntimeFiles(runtimeDir);
  }

  private async releaseRuntimeLock(): Promise<void> {
    const lock = this.runtimeLock;
    this.runtimeLock = null;
    if (lock) await lock.release();
  }
}

export function sanitizedSidecarEnv(
  source: NodeJS.ProcessEnv,
): NodeJS.ProcessEnv {
  // The companion is a high-privilege OAuth process, not a general child
  // shell. Inheriting arbitrary application variables would let legacy
  // OAuthAPI-LLM switches (HOME_JWT, MANAGEMENT_PASSWORD, store backends,
  // plugin controls, proxy variables, etc.) create a second control plane.
  // Keep only the small set required to start native helpers and to create
  // temporary files. Configuration and every secret travel through the
  // authenticated config/bootstrap channels instead.
  const allowed = new Set([
    "PATH",
    "SYSTEMROOT",
    "WINDIR",
    "COMSPEC",
    "PATHEXT",
    "TEMP",
    "TMP",
    "TMPDIR",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "TZ",
  ]);
  const clean: NodeJS.ProcessEnv = {};
  for (const [key, value] of Object.entries(source)) {
    if (allowed.has(key.toUpperCase()) && value !== undefined) clean[key] = value;
  }
  return clean;
}

export function accountBridgeStdio(platform: NodeJS.Platform): StdioOptions {
  return platform === "win32"
    ? ["pipe", "pipe", "pipe"]
    : ["ignore", "pipe", "pipe", "pipe"];
}

function platformTag(): string {
  if (process.platform === "darwin" && process.arch === "arm64") return "arm64-darwin";
  if (process.platform === "darwin" && process.arch === "x64") return "x64-darwin";
  if (process.platform === "linux" && process.arch === "arm64") return "arm64-linux";
  if (process.platform === "linux" && process.arch === "x64") return "x64-linux";
  if (process.platform === "win32" && process.arch === "x64") return "x64-win32";
  throw new AccountBridgeError("unsupported-platform");
}

function injectedComponentVersion(): string {
  let value: unknown;
  try {
    value =
      typeof ACCOUNT_BRIDGE_COMPONENT_VERSION === "undefined"
        ? undefined
        : ACCOUNT_BRIDGE_COMPONENT_VERSION;
  } catch {
    value = undefined;
  }
  if (
    typeof value !== "string" ||
    !/^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?$/.test(value)
  ) {
    throw new AccountBridgeError("component-version-unavailable");
  }
  return value;
}

function injectedCrabCodeReleaseVersion(): string {
  let value: unknown;
  try {
    value =
      typeof ACCOUNT_BRIDGE_CRABCODE_RELEASE_VERSION === "undefined"
        ? undefined
        : ACCOUNT_BRIDGE_CRABCODE_RELEASE_VERSION;
  } catch {
    value = undefined;
  }
  if (!isCanonicalReleaseVersion(value)) {
    throw new AccountBridgeError("crabcode-release-version-unavailable");
  }
  return value;
}

function injectedProtocolVersion(): number {
  let value: unknown;
  try {
    value =
      typeof ACCOUNT_BRIDGE_PROTOCOL_VERSION === "undefined"
        ? undefined
        : ACCOUNT_BRIDGE_PROTOCOL_VERSION;
  } catch {
    value = undefined;
  }
  if (!Number.isSafeInteger(value) || Number(value) <= 0) {
    throw new AccountBridgeError("protocol-version-unavailable");
  }
  return Number(value);
}

function injectedString(
  value: unknown,
  errorCode: string,
): string {
  if (typeof value !== "string" || value.length === 0) {
    throw new AccountBridgeError(errorCode);
  }
  return value;
}

async function sha256File(path: string): Promise<string> {
  return await new Promise((resolveDigest, reject) => {
    const hash = createHash("sha256");
    const stream = createReadStream(path);
    stream.on("data", chunk => hash.update(chunk));
    stream.on("error", reject);
    stream.on("end", () => resolveDigest(hash.digest("hex")));
  });
}

async function regularFileNoSymlink(path: string): Promise<void> {
  const stat = await fs.lstat(path);
  if (!stat.isFile() || stat.isSymbolicLink()) {
    throw new AccountBridgeError("artifact-layout-invalid");
  }
}

async function regularDirectoryNoSymlink(path: string): Promise<void> {
  const stat = await fs.lstat(path);
  if (!stat.isDirectory() || stat.isSymbolicLink()) {
    throw new AccountBridgeError("artifact-layout-invalid");
  }
}

async function requirePackagedPath(check: () => Promise<void>): Promise<void> {
  try {
    await check();
  } catch (error) {
    if (error instanceof AccountBridgeError) throw error;
    const code = (error as NodeJS.ErrnoException | null)?.code;
    if (code === "ENOENT" || code === "ENOTDIR") {
      throw new AccountBridgeError("artifact-missing");
    }
    throw error;
  }
}

async function packagedFileMustExist(path: string): Promise<void> {
  await requirePackagedPath(() => regularFileNoSymlink(path));
}

async function packagedDirectoryMustExist(path: string): Promise<void> {
  await requirePackagedPath(() => regularDirectoryNoSymlink(path));
}

function accountBridgeLicenseTarget(platform: string): string {
  const targets: Record<string, string> = {
    "arm64-darwin": "darwin/arm64",
    "x64-darwin": "darwin/amd64",
    "arm64-linux": "linux/arm64",
    "x64-linux": "linux/amd64",
    "x64-win32": "windows/amd64",
  };
  const target = targets[platform];
  if (!target) throw new AccountBridgeError("artifact-layout-invalid");
  return target;
}

type PackagedLicenseMaterial = {
  path: string;
  sha256: string;
  size: number;
};

async function packagedLicenseMaterialInventory(
  metadataDir: string,
): Promise<PackagedLicenseMaterial[]> {
  const root = join(metadataDir, LICENSE_MATERIALS_DIRECTORY);
  await packagedDirectoryMustExist(root);
  const files: PackagedLicenseMaterial[] = [];
  let totalBytes = 0;
  const visit = async (directory: string, prefix: string): Promise<void> => {
    const entries = (await fs.readdir(directory, { withFileTypes: true })).sort(
      (left, right) =>
        left.name < right.name ? -1 : left.name > right.name ? 1 : 0,
    );
    if (entries.length === 0) {
      throw new AccountBridgeError("license-materials-invalid");
    }
    for (const entry of entries) {
      const path = join(directory, entry.name);
      const metadata = await fs.lstat(path);
      const relativePath =
        prefix === "" ? entry.name : `${prefix}/${entry.name}`;
      const bundledPath = `${LICENSE_MATERIALS_DIRECTORY}/${relativePath}`;
      if (
        bundledPath.includes("\\") ||
        bundledPath.split("/").some(segment => segment === "" || segment === "." || segment === "..") ||
        Buffer.byteLength(bundledPath, "utf8") > MAX_LICENSE_MATERIAL_PATH_BYTES ||
        metadata.isSymbolicLink() ||
        entry.isSymbolicLink()
      ) {
        throw new AccountBridgeError("license-materials-invalid");
      }
      if (metadata.isDirectory() && entry.isDirectory()) {
        await visit(path, relativePath);
        continue;
      }
      if (
        !metadata.isFile() ||
        !entry.isFile() ||
        metadata.size <= 0 ||
        metadata.size > MAX_LICENSE_MATERIAL_FILE_BYTES
      ) {
        throw new AccountBridgeError("license-materials-invalid");
      }
      totalBytes += metadata.size;
      if (
        files.length >= MAX_LICENSE_MATERIAL_FILES ||
        totalBytes > MAX_LICENSE_MATERIAL_TOTAL_BYTES
      ) {
        throw new AccountBridgeError("license-materials-invalid");
      }
      files.push({
        path: bundledPath,
        sha256: await sha256File(path),
        size: metadata.size,
      });
    }
  };
  await visit(root, "");
  files.sort((left, right) =>
    left.path < right.path ? -1 : left.path > right.path ? 1 : 0,
  );
  return files;
}

async function verifyPackagedLicenseMaterials(
  metadataDir: string,
  expectedPlatform: string,
  manifestRaw: Buffer,
): Promise<void> {
  let manifest: unknown;
  try {
    manifest = JSON.parse(manifestRaw.toString("utf8"));
  } catch {
    throw new AccountBridgeError("license-materials-invalid");
  }
  if (
    !isRecord(manifest) ||
    JSON.stringify(Object.keys(manifest).sort()) !==
      JSON.stringify(["files", "scanner", "schemaVersion", "target"]) ||
    manifest.schemaVersion !== 1 ||
    manifest.target !== accountBridgeLicenseTarget(expectedPlatform) ||
    !isRecord(manifest.scanner) ||
    JSON.stringify(Object.keys(manifest.scanner).sort()) !==
      JSON.stringify(["module", "version"]) ||
    manifest.scanner.module !== "github.com/google/go-licenses" ||
    manifest.scanner.version !== "v1.6.0" ||
    !Array.isArray(manifest.files) ||
    manifest.files.length === 0 ||
    manifest.files.length > MAX_LICENSE_MATERIAL_FILES ||
    manifestRaw.toString("utf8") !== `${JSON.stringify(manifest, null, 2)}\n`
  ) {
    throw new AccountBridgeError("license-materials-invalid");
  }
  let previousPath = "";
  let totalBytes = 0;
  for (const file of manifest.files) {
    if (
      !isRecord(file) ||
      JSON.stringify(Object.keys(file).sort()) !==
        JSON.stringify(["path", "sha256", "size"]) ||
      typeof file.path !== "string" ||
      !file.path.startsWith(`${LICENSE_MATERIALS_DIRECTORY}/`) ||
      file.path.includes("\\") ||
      file.path.split("/").some(segment => segment === "" || segment === "." || segment === "..") ||
      Buffer.byteLength(file.path, "utf8") > MAX_LICENSE_MATERIAL_PATH_BYTES ||
      file.path <= previousPath ||
      typeof file.sha256 !== "string" ||
      !/^[a-f0-9]{64}$/.test(file.sha256) ||
      typeof file.size !== "number" ||
      !Number.isSafeInteger(file.size) ||
      file.size <= 0 ||
      file.size > MAX_LICENSE_MATERIAL_FILE_BYTES
    ) {
      throw new AccountBridgeError("license-materials-invalid");
    }
    previousPath = file.path;
    totalBytes += file.size;
  }
  if (totalBytes > MAX_LICENSE_MATERIAL_TOTAL_BYTES) {
    throw new AccountBridgeError("license-materials-invalid");
  }
  const actual = await packagedLicenseMaterialInventory(metadataDir);
  if (JSON.stringify(actual) !== JSON.stringify(manifest.files)) {
    throw new AccountBridgeError("license-materials-invalid");
  }
}

function isExactProvenanceMaterial(
  value: unknown,
  expectedPath: string,
  expectedSha256: string,
): boolean {
  return (
    isRecord(value) &&
    JSON.stringify(Object.keys(value).sort()) ===
      JSON.stringify(["path", "sha256"]) &&
    value.path === expectedPath &&
    typeof value.sha256 === "string" &&
    /^[a-f0-9]{64}$/.test(value.sha256) &&
    value.sha256 === expectedSha256
  );
}

export async function verifyPackagedArtifact(input: {
  binaryPath: string;
  metadataDir: string;
  expectedComponentVersion: string;
  expectedPlatform: string;
  expectedProtocolVersion: number;
  artifactPublicKeyBase64URL: string;
  eligibilityPublicKeyBase64URL: string;
}): Promise<AccountBridgeArtifact> {
  const binaryName = input.expectedPlatform.endsWith("win32")
    ? "oauthapi-llm.exe"
    : "oauthapi-llm";
  if (basename(input.binaryPath) !== binaryName) {
    throw new AccountBridgeError("artifact-layout-invalid");
  }
  if (
    basename(input.metadataDir) !== "account-bridge" ||
    dirname(input.metadataDir) !== dirname(input.binaryPath)
  ) {
    throw new AccountBridgeError("artifact-layout-invalid");
  }
  await packagedDirectoryMustExist(dirname(input.binaryPath));
  await packagedDirectoryMustExist(input.metadataDir);
  await packagedFileMustExist(input.binaryPath);
  const pluginFileName = input.expectedPlatform.endsWith("win32")
    ? "gemini-cli.dll"
    : input.expectedPlatform.endsWith("darwin")
      ? "gemini-cli.dylib"
      : "gemini-cli.so";
  const pluginRelativePath = `plugins/${pluginFileName}`;
  const pluginPath = join(input.metadataDir, "plugins", pluginFileName);
  const pluginLicenseRelativePath = "plugins/gemini-cli-LICENSE";
  const pluginLicensePath = join(input.metadataDir, pluginLicenseRelativePath);
  await packagedFileMustExist(pluginPath);
  await packagedFileMustExist(pluginLicensePath);
  for (const name of [
    "LICENSE",
    "NOTICE",
    "THIRD_PARTY_NOTICES.md",
    LICENSE_MATERIALS_MANIFEST,
    "UPSTREAM.lock",
    "sbom.cdx.json",
    "signature.json",
    "provenance.json",
    "provenance.sig",
  ]) {
    await packagedFileMustExist(join(input.metadataDir, name));
  }
  const platformEvidenceName = input.expectedPlatform.endsWith("darwin")
    ? "notarization.json"
    : input.expectedPlatform.endsWith("linux")
      ? "binary.minisig"
      : null;
  if (platformEvidenceName !== null) {
    await packagedFileMustExist(join(input.metadataDir, platformEvidenceName));
  }
  if (input.expectedPlatform.endsWith("linux")) {
    await packagedFileMustExist(
      join(input.metadataDir, "plugins", "gemini-cli.minisig"),
    );
  }
  const lockRaw = await fs.readFile(join(input.metadataDir, "UPSTREAM.lock"));
  const provenanceRaw = await fs.readFile(join(input.metadataDir, "provenance.json"));
  const signatureEvidenceRaw = await fs.readFile(
    join(input.metadataDir, "signature.json"),
  );
  const provenanceSignatureRaw = await fs.readFile(
    join(input.metadataDir, "provenance.sig"),
  );
  const sbomRaw = await fs.readFile(join(input.metadataDir, "sbom.cdx.json"));
  const licenseMaterialsRaw = await fs.readFile(
    join(input.metadataDir, LICENSE_MATERIALS_MANIFEST),
  );
  const licenseDigest = await sha256File(join(input.metadataDir, "LICENSE"));
  const noticeDigest = await sha256File(join(input.metadataDir, "NOTICE"));
  const thirdPartyNoticesDigest = await sha256File(
    join(input.metadataDir, "THIRD_PARTY_NOTICES.md"),
  );
  const pluginLicenseDigest = await sha256File(pluginLicensePath);
  try {
    await verifyPackagedLicenseMaterials(
      input.metadataDir,
      input.expectedPlatform,
      licenseMaterialsRaw,
    );
    const signature = strictBase64URL(
      provenanceSignatureRaw.toString("utf8").trim(),
      64,
    );
    const artifactPublicKey = strictBase64URL(
      input.artifactPublicKeyBase64URL,
      32,
    );
    try {
      const publicKey = createPublicKey({
        key: Buffer.concat([
          Buffer.from("302a300506032b6570032100", "hex"),
          artifactPublicKey,
        ]),
        format: "der",
        type: "spki",
      });
      if (!verifySignature(null, provenanceRaw, publicKey, signature)) {
        throw new AccountBridgeError("artifact-signature-invalid");
      }
    } finally {
      signature.fill(0);
      artifactPublicKey.fill(0);
    }
    const lock = JSON.parse(lockRaw.toString("utf8")) as unknown;
    const provenance = JSON.parse(provenanceRaw.toString("utf8")) as unknown;
    const signatureEvidence = JSON.parse(
      signatureEvidenceRaw.toString("utf8"),
    ) as unknown;
    const sbom = JSON.parse(sbomRaw.toString("utf8")) as unknown;
    if (
      !isRecord(lock) ||
      lock.schemaVersion !== 1 ||
      lock.component !== COMPONENT ||
      lock.componentVersion !== input.expectedComponentVersion ||
      lock.protocolVersion !== input.expectedProtocolVersion ||
      lock.module !== COMPONENT_MODULE ||
      !isRecord(lock.upstream) ||
      lock.upstream.repository !== "https://github.com/router-for-me/CLIProxyAPI" ||
      typeof lock.upstream.release !== "string" ||
      !/^v\d+\.\d+\.\d+$/.test(lock.upstream.release) ||
      typeof lock.upstream.commit !== "string" ||
      !/^[a-f0-9]{40}$/.test(lock.upstream.commit) ||
      typeof lock.upstream.tree !== "string" ||
      !/^[a-f0-9]{40}$/.test(lock.upstream.tree) ||
      typeof lock.upstream.licenseSha256 !== "string" ||
      !/^[a-f0-9]{64}$/.test(lock.upstream.licenseSha256) ||
      lock.upstream.licenseSha256 !== licenseDigest ||
      !isRecord(lock.importPolicy) ||
      lock.importPolicy.editableTruth !== "components/oauthapi-llm" ||
      lock.importPolicy.runtimeDownloadsAllowed !== false ||
      lock.importPolicy.buildDownloadsRestrictedToLockedReleaseAssets !== true ||
      lock.importPolicy.pathFallbackAllowed !== false ||
      lock.importPolicy.nestedGitAllowed !== false ||
      lock.importPolicy.mirrorDirection !== "crabcode-to-acosmi-oauthapi-llm"
    ) {
      throw new AccountBridgeError("upstream-lock-invalid");
    }
    if (
      !isRecord(lock.fixedPlugins) ||
      JSON.stringify(Object.keys(lock.fixedPlugins).sort()) !==
        JSON.stringify(["googleGeminiCli"]) ||
      !isRecord(lock.fixedPlugins.googleGeminiCli)
    ) {
      throw new AccountBridgeError("fixed-plugin-lock-invalid");
    }
    const lockedPlugin = lock.fixedPlugins.googleGeminiCli;
    if (
      JSON.stringify(Object.keys(lockedPlugin).sort()) !==
        JSON.stringify([
          "commit",
          "commitSignature",
          "distributionStatus",
          "license",
          "licensePath",
          "licenseSha256",
          "release",
          "repository",
          "requiredEvidence",
          "targets",
        ]) ||
      lockedPlugin.repository !==
        "https://github.com/router-for-me/cpa-plugin-gemini-cli" ||
      typeof lockedPlugin.release !== "string" ||
      !/^v\d+\.\d+\.\d+$/.test(lockedPlugin.release) ||
      typeof lockedPlugin.commit !== "string" ||
      !/^[a-f0-9]{40}$/.test(lockedPlugin.commit) ||
      !isRecord(lockedPlugin.commitSignature) ||
      JSON.stringify(Object.keys(lockedPlugin.commitSignature).sort()) !==
        JSON.stringify(["gpgKeyId", "status"]) ||
      lockedPlugin.commitSignature.status !== "verified" ||
      typeof lockedPlugin.commitSignature.gpgKeyId !== "string" ||
      !/^[A-F0-9]{16}$/.test(lockedPlugin.commitSignature.gpgKeyId) ||
      lockedPlugin.license !== "MIT" ||
      lockedPlugin.licensePath !== "licenses/gemini-cli-LICENSE" ||
      lockedPlugin.licenseSha256 !== pluginLicenseDigest ||
      lockedPlugin.distributionStatus !== "fixed_release_assets_verified" ||
      JSON.stringify(lockedPlugin.requiredEvidence) !==
        JSON.stringify([
          "release-asset-sha256",
          "extracted-binary-sha256",
          "license-sha256",
          "sbom",
          "provenance",
        ]) ||
      !isRecord(lockedPlugin.targets) ||
      JSON.stringify(Object.keys(lockedPlugin.targets).sort()) !==
        JSON.stringify([
          "arm64-darwin",
          "arm64-linux",
          "x64-darwin",
          "x64-linux",
          "x64-win32",
        ])
    ) {
      throw new AccountBridgeError("fixed-plugin-lock-invalid");
    }
    const lockedPluginTargetValue = lockedPlugin.targets[input.expectedPlatform];
    if (!isRecord(lockedPluginTargetValue)) {
      throw new AccountBridgeError("fixed-plugin-lock-invalid");
    }
    const lockedPluginTarget = lockedPluginTargetValue;
    if (
      JSON.stringify(Object.keys(lockedPluginTarget).sort()) !==
        JSON.stringify(["archiveSha256", "asset", "binary", "binarySha256"]) ||
      typeof lockedPluginTarget.asset !== "string" ||
      !/^gemini-cli_\d+\.\d+\.\d+_[a-z0-9_]+\.zip$/.test(
        lockedPluginTarget.asset,
      ) ||
      typeof lockedPluginTarget.archiveSha256 !== "string" ||
      !/^[a-f0-9]{64}$/.test(lockedPluginTarget.archiveSha256) ||
      lockedPluginTarget.binary !== pluginFileName ||
      typeof lockedPluginTarget.binarySha256 !== "string" ||
      !/^[a-f0-9]{64}$/.test(lockedPluginTarget.binarySha256)
    ) {
      throw new AccountBridgeError("fixed-plugin-lock-invalid");
    }
    const lockDigest = createHash("sha256").update(lockRaw).digest("hex");
    const sbomDigest = createHash("sha256").update(sbomRaw).digest("hex");
    const signatureEvidenceDigest = createHash("sha256")
      .update(signatureEvidenceRaw)
      .digest("hex");
    const licenseMaterialsDigest = createHash("sha256")
      .update(licenseMaterialsRaw)
      .digest("hex");
    const windowsUnsigned = input.expectedPlatform.endsWith("win32");
    if (
      !isRecord(provenance) ||
      provenance.schemaVersion !== 1 ||
      provenance.component !== COMPONENT ||
      provenance.componentVersion !== input.expectedComponentVersion ||
      provenance.platform !== input.expectedPlatform ||
      provenance.protocolVersion !== input.expectedProtocolVersion ||
      typeof provenance.crabCodeCommit !== "string" ||
      !/^[a-f0-9]{40}$/.test(provenance.crabCodeCommit) ||
      typeof provenance.sourceTree !== "string" ||
      !/^[a-f0-9]{40}$/.test(provenance.sourceTree) ||
      provenance.upstreamLockSha256 !== lockDigest ||
      !isRecord(provenance.materials) ||
      JSON.stringify(Object.keys(provenance.materials).sort()) !==
        JSON.stringify([
          "fixedPluginLicense",
          "license",
          "notice",
          "sbom",
          "signatureEvidence",
          "thirdPartyLicenseMaterials",
          "thirdPartyNotices",
          "upstreamLock",
        ]) ||
      !isExactProvenanceMaterial(
        provenance.materials.fixedPluginLicense,
        pluginLicenseRelativePath,
        pluginLicenseDigest,
      ) ||
      !isExactProvenanceMaterial(
        provenance.materials.license,
        "LICENSE",
        licenseDigest,
      ) ||
      !isExactProvenanceMaterial(
        provenance.materials.notice,
        "NOTICE",
        noticeDigest,
      ) ||
      !isExactProvenanceMaterial(
        provenance.materials.thirdPartyNotices,
        "THIRD_PARTY_NOTICES.md",
        thirdPartyNoticesDigest,
      ) ||
      !isExactProvenanceMaterial(
        provenance.materials.thirdPartyLicenseMaterials,
        LICENSE_MATERIALS_MANIFEST,
        licenseMaterialsDigest,
      ) ||
      !isExactProvenanceMaterial(
        provenance.materials.upstreamLock,
        "UPSTREAM.lock",
        lockDigest,
      ) ||
      !isExactProvenanceMaterial(
        provenance.materials.sbom,
        "sbom.cdx.json",
        sbomDigest,
      ) ||
      !isExactProvenanceMaterial(
        provenance.materials.signatureEvidence,
        "signature.json",
        signatureEvidenceDigest,
      ) ||
      provenance.upstreamLockSha256 !== lockDigest ||
      !isRecord(provenance.sbom) ||
      JSON.stringify(Object.keys(provenance.sbom).sort()) !==
        JSON.stringify(["path", "sha256"]) ||
      provenance.sbom.path !== "sbom.cdx.json" ||
      provenance.sbom.sha256 !== sbomDigest ||
      !isRecord(provenance.binary) ||
      provenance.binary.path !== `bin/${binaryName}` ||
      typeof provenance.binary.sha256 !== "string" ||
      provenance.binary.sha256 !== (await sha256File(input.binaryPath)) ||
      !Array.isArray(provenance.fixedPlugins) ||
      !Array.isArray(provenance.runtimeHelpers) ||
      typeof provenance.signatureEvidenceSha256 !== "string" ||
      provenance.signatureEvidenceSha256 !== signatureEvidenceDigest ||
      (windowsUnsigned
        ? provenance.platformSignature !== null
        : !isRecord(provenance.platformSignature)) ||
      !isRecord(signatureEvidence) ||
      JSON.stringify(Object.keys(signatureEvidence).sort()) !==
        JSON.stringify([
          "binarySha256",
          "component",
          "componentVersion",
          "fixedPlugins",
          "platform",
          "platformSignature",
          "runtimeHelpers",
          "schemaVersion",
        ]) ||
      signatureEvidence.schemaVersion !== 1 ||
      signatureEvidence.component !== COMPONENT ||
      signatureEvidence.componentVersion !== input.expectedComponentVersion ||
      signatureEvidence.platform !== input.expectedPlatform ||
      signatureEvidence.binarySha256 !== provenance.binary.sha256 ||
      JSON.stringify(signatureEvidence.platformSignature) !==
        JSON.stringify(provenance.platformSignature) ||
      JSON.stringify(signatureEvidence.fixedPlugins) !==
        JSON.stringify(provenance.fixedPlugins) ||
      JSON.stringify(signatureEvidence.runtimeHelpers) !==
        JSON.stringify(provenance.runtimeHelpers) ||
      !isRecord(provenance.build) ||
      provenance.build.cgoEnabled !== false ||
      provenance.build.trimpath !== true ||
      provenance.build.buildvcs !== false ||
      provenance.build.sourceMode !== "git-archive" ||
      provenance.build.releaseState !== "signed" ||
      provenance.build.eligibilityTrustRootSha256 !==
        createHash("sha256")
          .update(Buffer.from(input.eligibilityPublicKeyBase64URL, "base64url"))
          .digest("hex") ||
      !isRecord(sbom) ||
      sbom.bomFormat !== "CycloneDX" ||
      !isRecord(sbom.metadata) ||
      !isRecord(sbom.metadata.component) ||
      sbom.metadata.component.name !== COMPONENT ||
      sbom.metadata.component.version !== input.expectedComponentVersion
    ) {
      throw new AccountBridgeError("provenance-invalid");
    }
    const platformSignature = provenance.platformSignature;
    const darwinSigned = input.expectedPlatform.endsWith("darwin");
    const expectedSignatureScheme = darwinSigned ? "apple-developer-id" : "minisign";
    if (windowsUnsigned) {
      if (platformSignature !== null) {
        throw new AccountBridgeError("platform-signature-evidence-invalid");
      }
    } else if (
      !isRecord(platformSignature) ||
      platformSignature.scheme !== expectedSignatureScheme
    ) {
      throw new AccountBridgeError("platform-signature-evidence-invalid");
    }
    if (!windowsUnsigned && darwinSigned) {
      if (!isRecord(platformSignature)) {
        throw new AccountBridgeError("platform-signature-evidence-invalid");
      }
      const isDeveloperIdSignature = (
        value: unknown,
      ): value is Record<string, unknown> =>
        isRecord(value) &&
        JSON.stringify(Object.keys(value).sort()) ===
          JSON.stringify([
            "detachedSignature",
            "notarizationEvidenceSha256",
            "scheme",
            "signerIdSha256",
          ]) &&
        value.scheme === expectedSignatureScheme &&
        value.detachedSignature === null &&
        typeof value.signerIdSha256 === "string" &&
        /^[a-f0-9]{64}$/.test(value.signerIdSha256) &&
        typeof value.notarizationEvidenceSha256 === "string" &&
        /^[a-f0-9]{64}$/.test(value.notarizationEvidenceSha256);
      if (!isDeveloperIdSignature(platformSignature)) {
        throw new AccountBridgeError("platform-signature-evidence-invalid");
      }

      const helperPath = join(dirname(input.binaryPath), "oauthapi-plugin-host");
      await packagedFileMustExist(helperPath);
      const helperDigest = await sha256File(helperPath);
      const runtimeHelper: unknown = provenance.runtimeHelpers[0];
      if (
        provenance.runtimeHelpers.length !== 1 ||
        !isRecord(runtimeHelper) ||
        JSON.stringify(Object.keys(runtimeHelper).sort()) !==
          JSON.stringify(["id", "path", "platformSignature", "sha256"]) ||
        runtimeHelper.id !== "plugin-host" ||
        runtimeHelper.path !== "bin/oauthapi-plugin-host" ||
        runtimeHelper.sha256 !== helperDigest ||
        !isDeveloperIdSignature(runtimeHelper.platformSignature) ||
        runtimeHelper.platformSignature.signerIdSha256 !==
          platformSignature.signerIdSha256 ||
        runtimeHelper.platformSignature.notarizationEvidenceSha256 !==
          platformSignature.notarizationEvidenceSha256
      ) {
        throw new AccountBridgeError("platform-signature-evidence-invalid");
      }
      const pluginEntry: unknown = provenance.fixedPlugins[0];
      if (
        !isRecord(pluginEntry) ||
        !isDeveloperIdSignature(pluginEntry.platformSignature) ||
        pluginEntry.platformSignature.signerIdSha256 !==
          platformSignature.signerIdSha256 ||
        pluginEntry.platformSignature.notarizationEvidenceSha256 !==
          platformSignature.notarizationEvidenceSha256
      ) {
        throw new AccountBridgeError("platform-signature-evidence-invalid");
      }
      const notarizationPath = join(input.metadataDir, "notarization.json");
      const notarizationRaw = await fs.readFile(notarizationPath);
      try {
        const notarization = JSON.parse(notarizationRaw.toString("utf8")) as unknown;
        if (
          platformSignature.notarizationEvidenceSha256 !==
            createHash("sha256").update(notarizationRaw).digest("hex") ||
          !isRecord(notarization) ||
          JSON.stringify(Object.keys(notarization).sort()) !==
            JSON.stringify([
              "artifacts",
              "schemaVersion",
              "status",
              "submissionId",
            ]) ||
          notarization.schemaVersion !== 1 ||
          notarization.status !== "Accepted" ||
          typeof notarization.submissionId !== "string" ||
          !/^[0-9a-f-]{36}$/i.test(notarization.submissionId) ||
          JSON.stringify(notarization.artifacts) !==
            JSON.stringify([
              {
                path: `bin/${binaryName}`,
                sha256: provenance.binary.sha256,
              },
              {
                path: "bin/oauthapi-plugin-host",
                sha256: helperDigest,
              },
              {
                path: pluginRelativePath,
                sha256: await sha256File(pluginPath),
              },
            ])
        ) {
          throw new AccountBridgeError("platform-signature-evidence-invalid");
        }
      } finally {
        notarizationRaw.fill(0);
      }
    } else if (!windowsUnsigned) {
      if (!isRecord(platformSignature)) {
        throw new AccountBridgeError("platform-signature-evidence-invalid");
      }
      if (
        JSON.stringify(Object.keys(platformSignature).sort()) !==
          JSON.stringify([
            "detachedSignature",
            "detachedSignatureSha256",
            "scheme",
            "signerIdSha256",
          ]) ||
        typeof platformSignature.signerIdSha256 !== "string" ||
        !/^[a-f0-9]{64}$/.test(platformSignature.signerIdSha256) ||
        platformSignature.detachedSignature !== "binary.minisig" ||
        typeof platformSignature.detachedSignatureSha256 !== "string" ||
        !/^[a-f0-9]{64}$/.test(platformSignature.detachedSignatureSha256) ||
        platformSignature.detachedSignatureSha256 !==
          (await sha256File(join(input.metadataDir, "binary.minisig")))
      ) {
        throw new AccountBridgeError("platform-signature-evidence-invalid");
      }
    }
    // Only signed macOS artifacts carry a runtime helper; Windows and Linux
    // builds must declare an exactly-empty list (mirrors build-account-bridge).
    if (
      !darwinSigned &&
      provenance.runtimeHelpers.length !== 0
    ) {
      throw new AccountBridgeError("provenance-invalid");
    }
    const fixedPlugins: AccountBridgeArtifact["fixedPlugins"] = [];
    if (provenance.fixedPlugins.length !== 1) {
      throw new AccountBridgeError("fixed-plugin-manifest-invalid");
    }
    for (const entry of provenance.fixedPlugins) {
      if (
        !isRecord(entry) ||
        JSON.stringify(Object.keys(entry).sort()) !==
          JSON.stringify([
            "id",
            "license",
            "path",
            "platformSignature",
            "sha256",
            "source",
          ]) ||
        entry.id !== "gemini-cli" ||
        entry.path !== pluginRelativePath ||
        typeof entry.sha256 !== "string" ||
        !/^[a-f0-9]{64}$/.test(entry.sha256) ||
        !isRecord(entry.source) ||
        JSON.stringify(Object.keys(entry.source).sort()) !==
          JSON.stringify([
            "archiveSha256",
            "asset",
            "binarySha256",
            "commit",
            "release",
            "repository",
          ]) ||
        entry.source.repository !== lockedPlugin.repository ||
        entry.source.release !== lockedPlugin.release ||
        entry.source.commit !== lockedPlugin.commit ||
        entry.source.asset !== lockedPluginTarget.asset ||
        entry.source.archiveSha256 !== lockedPluginTarget.archiveSha256 ||
        entry.source.binarySha256 !== lockedPluginTarget.binarySha256 ||
        !isRecord(entry.license) ||
        JSON.stringify(Object.keys(entry.license).sort()) !==
          JSON.stringify(["path", "sha256"]) ||
        entry.license.path !== pluginLicenseRelativePath ||
        entry.license.sha256 !== pluginLicenseDigest
      ) {
        throw new AccountBridgeError("fixed-plugin-manifest-invalid");
      }
      if ((await sha256File(pluginPath)) !== entry.sha256) {
        throw new AccountBridgeError("fixed-plugin-sha-invalid");
      }
      const pluginSignature = entry.platformSignature;
      if (windowsUnsigned) {
        if (pluginSignature !== null) {
          throw new AccountBridgeError("fixed-plugin-signature-evidence-invalid");
        }
      } else if (
        !isRecord(pluginSignature) ||
        !isRecord(platformSignature) ||
        pluginSignature.scheme !== expectedSignatureScheme ||
        pluginSignature.signerIdSha256 !== platformSignature.signerIdSha256
      ) {
        throw new AccountBridgeError("fixed-plugin-signature-evidence-invalid");
      } else if (darwinSigned) {
        if (
          JSON.stringify(Object.keys(pluginSignature).sort()) !==
            JSON.stringify([
              "detachedSignature",
              "notarizationEvidenceSha256",
              "scheme",
              "signerIdSha256",
            ]) ||
          pluginSignature.detachedSignature !== null ||
          typeof pluginSignature.signerIdSha256 !== "string" ||
          !/^[a-f0-9]{64}$/.test(pluginSignature.signerIdSha256) ||
          pluginSignature.notarizationEvidenceSha256 !==
            platformSignature.notarizationEvidenceSha256
        ) {
          throw new AccountBridgeError("fixed-plugin-signature-evidence-invalid");
        }
      } else {
        if (
          JSON.stringify(Object.keys(pluginSignature).sort()) !==
          JSON.stringify([
            "detachedSignature",
            "detachedSignatureSha256",
            "scheme",
            "signerIdSha256",
          ]) ||
          typeof pluginSignature.signerIdSha256 !== "string" ||
          !/^[a-f0-9]{64}$/.test(pluginSignature.signerIdSha256) ||
          pluginSignature.detachedSignature !== "plugins/gemini-cli.minisig" ||
          pluginSignature.detachedSignatureSha256 !==
            (await sha256File(
              join(input.metadataDir, "plugins", "gemini-cli.minisig"),
            ))
        ) {
          throw new AccountBridgeError("fixed-plugin-signature-evidence-invalid");
        }
      }
      if (!Array.isArray(sbom.components)) {
        throw new AccountBridgeError("fixed-plugin-sbom-invalid");
      }
      const pluginComponents = sbom.components.filter(
        component => isRecord(component) && component.name === "cpa-plugin-gemini-cli",
      );
      const sbomPlugin = pluginComponents[0];
      if (
        pluginComponents.length !== 1 ||
        !isRecord(sbomPlugin) ||
        sbomPlugin.type !== "library" ||
        sbomPlugin.version !== lockedPlugin.release.slice(1) ||
        sbomPlugin.purl !==
          `pkg:github/router-for-me/cpa-plugin-gemini-cli@${lockedPlugin.commit}` ||
        JSON.stringify(sbomPlugin.hashes) !==
          JSON.stringify([{ alg: "SHA-256", content: entry.sha256 }]) ||
        JSON.stringify(sbomPlugin.licenses) !==
          JSON.stringify([{ license: { id: "MIT" } }]) ||
        JSON.stringify(sbomPlugin.externalReferences) !==
          JSON.stringify([
            {
              type: "distribution",
              url: `${lockedPlugin.repository}/releases/download/${lockedPlugin.release}/${lockedPluginTarget.asset}`,
              hashes: [
                { alg: "SHA-256", content: lockedPluginTarget.archiveSha256 },
              ],
            },
          ]) ||
        JSON.stringify(sbomPlugin.properties) !==
          JSON.stringify([
            { name: "crabcode:bundled-path", value: pluginRelativePath },
            {
              name: "crabcode:upstream-extracted-binary-sha256",
              value: lockedPluginTarget.binarySha256,
            },
            {
              name: "crabcode:license-sha256",
              value: pluginLicenseDigest,
            },
          ])
      ) {
        throw new AccountBridgeError("fixed-plugin-sbom-invalid");
      }
      fixedPlugins.push({
        id: "gemini-cli",
        path: pluginPath,
        sha256: entry.sha256,
      });
    }
    return {
      binaryPath: input.binaryPath,
      metadataDir: input.metadataDir,
      componentVersion: input.expectedComponentVersion,
      protocolVersion: input.expectedProtocolVersion,
      fixedPlugins,
    };
  } finally {
    lockRaw.fill(0);
    provenanceRaw.fill(0);
    signatureEvidenceRaw.fill(0);
    provenanceSignatureRaw.fill(0);
    sbomRaw.fill(0);
    licenseMaterialsRaw.fill(0);
  }
}

/**
 * Resolve only the layouts owned by this repository/release pipeline.
 *
 * The manager runs only from the native TUI bundle or its raw-source
 * development layout:
 * - `<package>/dist/tui-runtime/index.js` in a release;
 * - `<repo>/src/services/accountBridge/*.ts` during development.
 *
 * There is deliberately no PATH lookup, cwd fallback, environment override or
 * ancestor scan. An unknown layout is an integrity failure, not an invitation
 * to execute a nearby binary.
 */
export function resolveAccountBridgePackageRoot(moduleURL: string): string {
  const moduleDir = dirname(fileURLToPath(moduleURL));
  if (
    basename(moduleDir) === "tui-runtime" &&
    basename(dirname(moduleDir)) === "dist"
  ) {
    return resolve(moduleDir, "..", "..");
  }
  if (
    basename(moduleDir) === "accountBridge" &&
    basename(dirname(moduleDir)) === "services" &&
    basename(dirname(dirname(moduleDir))) === "src"
  ) {
    return resolve(moduleDir, "..", "..", "..");
  }
  throw new AccountBridgeError("artifact-layout-invalid");
}

function packagedArtifactPaths(): { binaryPath: string; metadataDir: string } {
  const packageRoot = resolveAccountBridgePackageRoot(import.meta.url);
  const binaryName = process.platform === "win32" ? "oauthapi-llm.exe" : "oauthapi-llm";
  return {
    binaryPath: join(packageRoot, "bin", binaryName),
    metadataDir: join(packageRoot, "bin", "account-bridge"),
  };
}

function configRoot(): string {
  const explicit = process.env.CRABCODE_CONFIG_DIR?.trim();
  if (explicit) return resolve(explicit);
  const home = process.env.CRABCODE_HOME?.trim();
  return home ? resolve(home, ".crabcode") : join(homedir(), ".crabcode");
}

export async function writeAccountBridgeDiagnosticBundle(
  bundle: AccountBridgeLocalDiagnosticBundle,
  root = configRoot(),
): Promise<void> {
  const diagnosticsDir = join(root, "diagnostics");
  await fs.mkdir(diagnosticsDir, { recursive: true, mode: 0o700 });
  await regularDirectoryNoSymlink(diagnosticsDir);
  await fs.chmod(diagnosticsDir, 0o700);

  const serialized = Buffer.from(`${JSON.stringify(bundle, null, 2)}\n`, "utf8");
  try {
    if (serialized.byteLength > 4 << 20) {
      throw new AccountBridgeError("diagnostic-bundle-too-large");
    }
    // Defense in depth: the typed builder does not define any of these keys.
    // Reject instead of persisting if a future refactor tries to add one.
    if (
      /"(?:token|tokens|accessToken|access_token|refreshToken|refresh_token|oauthToken|oauth_token|email|subject|rawSubject|raw_subject|key|keys|apiKey|api_key|managementKey|management_key|inferenceKey|inference_key|grant|grants|signedGrant|signed_grant|eligibilityGrant|eligibility_grant)"\s*:/i.test(
        serialized.toString("utf8"),
      )
    ) {
      throw new AccountBridgeError("diagnostic-bundle-sensitive-field");
    }
    const suffix = randomBytes(12).toString("hex");
    const temporaryPath = join(
      diagnosticsDir,
      `.account-bridge.${process.pid}.${suffix}.tmp`,
    );
    const targetPath = join(diagnosticsDir, "account-bridge.json");
    try {
      await fs.writeFile(temporaryPath, serialized, { mode: 0o600, flag: "wx" });
      await fs.chmod(temporaryPath, 0o600);
      let existing: Awaited<ReturnType<typeof fs.lstat>> | null = null;
      try {
        existing = await fs.lstat(targetPath);
      } catch (error) {
        if ((error as NodeJS.ErrnoException).code !== "ENOENT") throw error;
      }
      if (existing && (!existing.isFile() || existing.isSymbolicLink())) {
        throw new AccountBridgeError("diagnostic-target-invalid");
      }
      if (existing) await fs.rm(targetPath, { force: true });
      await fs.rename(temporaryPath, targetPath);
      await fs.chmod(targetPath, 0o600);
    } finally {
      await fs.rm(temporaryPath, { force: true }).catch(() => {});
    }
  } finally {
    serialized.fill(0);
  }
}

async function acquireProductionLock(): Promise<RuntimeLock | null> {
  const lockDir = join(configRoot(), ".account-bridge-runtime.lock");
  await fs.mkdir(dirname(lockDir), { recursive: true, mode: 0o700 });
  for (let attempt = 0; attempt < 2; attempt++) {
    try {
      await fs.mkdir(lockDir, { mode: 0o700 });
      await fs.writeFile(
        join(lockDir, "owner.json"),
        JSON.stringify({ pid: process.pid, acquiredAt: Date.now() }),
        { mode: 0o600, flag: "wx" },
      );
      return {
        async release() {
          let owned = false;
          try {
            const owner = JSON.parse(
              await fs.readFile(join(lockDir, "owner.json"), "utf8"),
            ) as unknown;
            owned = isRecord(owner) && owner.pid === process.pid;
          } catch {
            owned = false;
          }
          if (owned) await fs.rm(lockDir, { recursive: true, force: true });
        },
      };
    } catch (error) {
      if (!isRecord(error) || error.code !== "EEXIST") {
        throw new AccountBridgeError("runtime-lock-failed");
      }
      let ownerPid = 0;
      try {
        const owner = JSON.parse(
          await fs.readFile(join(lockDir, "owner.json"), "utf8"),
        ) as unknown;
        ownerPid = isRecord(owner) && Number.isSafeInteger(owner.pid) ? Number(owner.pid) : 0;
      } catch {
        ownerPid = 0;
      }
      let alive = false;
      if (ownerPid > 1) {
        try {
          process.kill(ownerPid, 0);
          alive = true;
        } catch {
          alive = false;
        }
      }
      if (alive) return null;
      await fs.rm(lockDir, { recursive: true, force: true });
    }
  }
  return null;
}

async function createProductionRuntimeFiles(input: {
  managementKey: string;
  inferenceKey: string;
  fixedPlugins: AccountBridgeArtifact["fixedPlugins"];
}): Promise<{ configPath: string; runtimeDir: string }> {
  const bridgeRoot = join(configRoot(), "account-bridge");
  await fs.mkdir(bridgeRoot, { recursive: true, mode: 0o700 });
  await regularDirectoryNoSymlink(bridgeRoot);
  await fs.chmod(bridgeRoot, 0o700);
  const runtimeDir = await fs.mkdtemp(join(bridgeRoot, "runtime-"));
  const authDir = join(bridgeRoot, "auth");
  await regularDirectoryNoSymlink(runtimeDir);
  await fs.chmod(runtimeDir, 0o700);
  await fs.mkdir(authDir, { recursive: true, mode: 0o700 });
  await regularDirectoryNoSymlink(authDir);
  await fs.chmod(authDir, 0o700);
  const configPath = join(runtimeDir, "config.json");
  const config = {
    host: "127.0.0.1",
    port: 0,
    "auth-dir": authDir,
    "api-keys": [input.inferenceKey],
    "remote-management": {
      "allow-remote": false,
      "secret-key": input.managementKey,
      "disable-control-panel": true,
      "disable-auto-update-panel": true,
      "panel-github-repository": "",
    },
    plugins:
      input.fixedPlugins.length === 0
        ? { enabled: false, configs: {} }
        : {
            enabled: true,
            // Artifact verification above permits exactly the fixed
            // gemini-cli binary and validates its signed-provenance SHA. The
            // Go boundary independently enforces the same one-plugin allowlist.
            dir: dirname(input.fixedPlugins[0]!.path),
            configs: { "gemini-cli": { enabled: true } },
          },
    pprof: { enable: false },
    "ws-auth": true,
    "usage-statistics-enabled": false,
    "proxy-url": "direct",
    "disable-claude-cloak-mode": true,
    codex: { "identity-confuse": false },
  };
  await fs.writeFile(configPath, JSON.stringify(config), {
    mode: 0o600,
    flag: "wx",
  });
  await fs.chmod(configPath, 0o600);
  return { configPath, runtimeDir };
}

export const SIDECAR_FORENSIC_LINE_CAP = 32;
export const SIDECAR_FORENSIC_LINE_MAX_BYTES = 512;

const SIDECAR_BANNER_PATTERN = /^OAuthAPI-LLM Version: [ -~]{1,200}$/;
const SIDECAR_LOG_LINE_PATTERN =
  /^\[([^\]]{1,40})\] \[[^\]]{0,120}\] \[([^\]]{1,16})\] \[([^\]]{1,80})\] ([\s\S]*)$/;
const SIDECAR_TIMESTAMP_PATTERN = /^[0-9 :.\-TZ+]{1,40}$/;
const SIDECAR_LEVEL_PATTERN = /^[a-z]{1,10}$/;
const SIDECAR_ORIGIN_PATTERN = /^[A-Za-z0-9_.+-]{1,64}:\d{1,7}$/;
const SIDECAR_MESSAGE_ALLOWLIST_PREFIXES = [
  "Account Bridge ",
  "pluginhost: ",
  "failed to ",
] as const;
const SIDECAR_STABLE_FAILURE_ANCHORS = [
  [/^failed to resolve auth directory\b/, "failed to resolve auth directory"],
  [/^failed to read auth file\b/, "failed to read auth file"],
  [/^failed to parse auth file\b/, "failed to parse auth file"],
  [/^failed to load config\b/, "failed to load config"],
  [/^failed to reload config\b/, "failed to reload config"],
  [/^failed to configure log output\b/, "failed to configure log output"],
] as const;
const SIDECAR_OPAQUE_RUN_PATTERN = /[A-Za-z0-9_-]{20,}/g;

export function redactSidecarLogMessage(message: string): {
  message: string | null;
  redactedByteLength: number | null;
} {
  const byteLength = Buffer.byteLength(message, "utf8");
  if (
    !SIDECAR_MESSAGE_ALLOWLIST_PREFIXES.some(prefix =>
      message.startsWith(prefix),
    ) ||
    !/^[ -!#-~]*$/.test(message)
  ) {
    return { message: null, redactedByteLength: byteLength };
  }
  if (message.startsWith("failed to ")) {
    // `failed to …` is emitted across the watcher/config/auth stack and its
    // free-form tail can contain a basename (including a short email address)
    // without any slash for path redaction to notice. Retain only enumerated
    // operation anchors. Unknown operations collapse to one stable category;
    // origin + level + exit evidence still identify the failing branch.
    const stable = SIDECAR_STABLE_FAILURE_ANCHORS.find(([pattern]) =>
      pattern.test(message),
    );
    return {
      message: stable?.[1] ?? "sidecar operation failed",
      redactedByteLength: null,
    };
  }
  if (message.startsWith("pluginhost: ")) {
    // Plugin-host failures may embed the selected auth identifier in otherwise
    // stable-looking text. Keep only the subsystem/branch classification.
    return {
      message: "pluginhost operation failed",
      redactedByteLength: null,
    };
  }
  if (message.startsWith("Account Bridge bootstrap rejected")) {
    // Bootstrap rejection details are free-form and can include short account
    // identifiers that do not resemble paths or opaque tokens.
    return {
      message: "Account Bridge bootstrap rejected",
      redactedByteLength: null,
    };
  }
  // Free-form log text has no quoting contract, so a whitespace-delimited path
  // regex cannot know whether `My File/token.json` ends at `My` or at the end
  // of the line. Once any path separator appears, retain only the stable
  // allowlisted prefix before that token and redact the entire remaining tail.
  // This is deliberately lossy: forensics may keep an error branch anchor, but
  // must never retain the second half of a path containing spaces.
  const firstPathSeparator = message.search(/[\\/]/);
  const pathTokenStart =
    firstPathSeparator === -1
      ? -1
      : message.lastIndexOf(" ", firstPathSeparator) + 1;
  const withoutPath =
    pathTokenStart === -1
      ? message
      : `${message.slice(0, pathTokenStart)}<path>`;
  const redacted = withoutPath
    .replace(SIDECAR_OPAQUE_RUN_PATTERN, run => `<redacted:${run.length}>`);
  return redacted.length <= 256
    ? { message: redacted, redactedByteLength: null }
    : { message: null, redactedByteLength: byteLength };
}

export function parseSidecarLogLine(
  line: string,
  stream: "stdout" | "stderr",
): AccountBridgeSidecarLogLine {
  const byteLength = Buffer.byteLength(line, "utf8");
  const withheld = (): AccountBridgeSidecarLogLine => ({
    stream,
    timestamp: null,
    level: null,
    origin: null,
    message: null,
    redactedByteLength: byteLength,
  });
  if (byteLength > SIDECAR_FORENSIC_LINE_MAX_BYTES) return withheld();
  const match = SIDECAR_LOG_LINE_PATTERN.exec(line);
  if (!match) return withheld();
  const [, rawTimestamp = "", rawLevel = "", rawOrigin = "", rawMessage = ""] =
    match;
  const { message, redactedByteLength } = redactSidecarLogMessage(rawMessage);
  return {
    stream,
    timestamp: SIDECAR_TIMESTAMP_PATTERN.test(rawTimestamp)
      ? rawTimestamp
      : null,
    level: SIDECAR_LEVEL_PATTERN.test(rawLevel) ? rawLevel : null,
    origin: SIDECAR_ORIGIN_PATTERN.test(rawOrigin) ? rawOrigin : null,
    message,
    redactedByteLength,
  };
}

interface SidecarForensicRecorder {
  recordLine(line: string, stream: "stdout" | "stderr"): void;
  recordExit(code: number | null, signal: NodeJS.Signals | null): void;
  recordSpawnError(error: unknown): void;
  markReadyLineRejected(): void;
  snapshot(): AccountBridgeSidecarForensics;
}

function createSidecarForensicRecorder(): SidecarForensicRecorder {
  const state: AccountBridgeSidecarForensics = {
    exitCode: null,
    exitSignal: null,
    spawnErrorCode: null,
    bannerLine: null,
    logLines: [],
    droppedLineCount: 0,
    readyLineRejected: false,
  };
  return {
    recordLine: (line, stream) => {
      if (
        state.bannerLine === null &&
        stream === "stdout" &&
        SIDECAR_BANNER_PATTERN.test(line)
      ) {
        state.bannerLine = line;
        return;
      }
      if (state.logLines.length >= SIDECAR_FORENSIC_LINE_CAP) {
        state.droppedLineCount += 1;
        return;
      }
      state.logLines.push(parseSidecarLogLine(line, stream));
    },
    recordExit: (code, signal) => {
      state.exitCode = Number.isSafeInteger(code) ? code : null;
      state.exitSignal =
        typeof signal === "string" && /^SIG[A-Z0-9]{1,12}$/.test(signal)
          ? signal
          : null;
    },
    recordSpawnError: error => {
      const code = (error as NodeJS.ErrnoException | null)?.code;
      state.spawnErrorCode =
        typeof code === "string" && /^[A-Z][A-Z0-9_]{1,31}$/.test(code)
          ? code
          : null;
    },
    markReadyLineRejected: () => {
      state.readyLineRejected = true;
    },
    snapshot: () => ({
      ...state,
      logLines: state.logLines.map(entry => ({ ...entry })),
    }),
  };
}

export function childProcessAdapter(
  child: ChildProcess,
  platform: NodeJS.Platform,
  expectedProtocolVersion: number,
): AccountBridgeChild {
  const forensics = createSidecarForensicRecorder();
  const exited = new Promise<number | null>(resolveExit => {
    child.once("exit", (code, signal) => {
      forensics.recordExit(code, signal);
      resolveExit(code);
    });
    child.once("error", error => {
      forensics.recordSpawnError(error);
      resolveExit(null);
    });
  });
  const stderr = child.stderr;
  if (stderr) {
    const stderrLines = createInterface({ input: stderr });
    stderrLines.on("line", line => forensics.recordLine(line, "stderr"));
    stderrLines.on("error", () => {});
    stderr.on("error", () => {});
  }
  const endpoint = new Promise<string>((resolveEndpoint, reject) => {
    const stdout = child.stdout;
    if (!stdout) {
      reject(new AccountBridgeError("runtime-readiness-unavailable"));
      return;
    }
    const lines = createInterface({ input: stdout });
    lines.on("error", () => {});
    stdout.on("error", () => {});
    let settled = false;
    let acceptedReadyLine: string | null = null;
    const timer = setTimeout(() => {
      if (settled) return;
      settled = true;
      lines.close();
      stdout.resume();
      reject(new AccountBridgeError("runtime-readiness-timeout"));
    }, READY_TIMEOUT_MS);
    const done = (value?: string, error?: AccountBridgeError): void => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      if (value) {
        // Keep readline attached as a sink after readiness so stdout can never
        // fill its pipe and deadlock the sidecar. The line callback is guarded
        // by `settled` and performs no logging.
        resolveEndpoint(value);
      } else {
        lines.close();
        stdout.resume();
        reject(error ?? new AccountBridgeError("runtime-readiness-failed"));
      }
    };
    lines.on("line", line => {
      try {
        const value = parseAccountBridgeReadyLine(
          line,
          expectedProtocolVersion,
        );
        if (!value) {
          forensics.recordLine(line, "stdout");
          return;
        }
        if (acceptedReadyLine !== null) {
          // Duplicate (identical or drifted) readiness is impossible in the
          // single-listener process contract and indicates stream confusion.
          forensics.markReadyLineRejected();
          child.kill();
          return;
        }
        acceptedReadyLine = line;
        if (!settled) done(value);
      } catch {
        forensics.markReadyLineRejected();
        child.kill();
        done(undefined, new AccountBridgeError("runtime-readiness-invalid"));
        return;
      }
    });
    void exited.then(() => done(undefined, new AccountBridgeError("runtime-exited-before-ready")));
  });
  return {
    pid: child.pid ?? 0,
    endpoint,
    exited,
    terminate: () => {
      signalAccountBridgeProcessTree(child, platform, "SIGTERM");
    },
    kill: () => {
      signalAccountBridgeProcessTree(child, platform, "SIGKILL");
    },
    forensics: forensics.snapshot,
  };
}

function signalAccountBridgeProcessTree(
  child: ChildProcess,
  platform: NodeJS.Platform,
  signal: "SIGTERM" | "SIGKILL",
): void {
  if (platform === "win32") {
    // The direct child is the process-tree-exec Job owner selected by
    // accountBridgeSpawnCommand. Terminating it closes the kill-on-close Job
    // handle and synchronously makes the whole contained tree non-survivable.
    child.kill();
    return;
  }
  const pid = child.pid;
  if (typeof pid === "number" && pid > 0) {
    try {
      process.kill(-pid, signal);
      return;
    } catch {
      // A concurrently-exited process group is confirmed by `exited`; the
      // direct-handle fallback below handles runtimes that rejected detached
      // group creation.
    }
  }
  child.kill(signal);
}

export function parseAccountBridgeReadyLine(
  line: string,
  expectedProtocolVersion: number,
): string | null {
  let value: unknown;
  try {
    value = JSON.parse(line);
  } catch {
    return null;
  }
  if (!isRecord(value) || value.event !== "account-bridge-ready") return null;
  if (
    JSON.stringify(Object.keys(value).sort()) !==
      JSON.stringify(["address", "event", "port", "protocolVersion"]) ||
    value.protocolVersion !== expectedProtocolVersion ||
    value.address !== "127.0.0.1" ||
    !Number.isSafeInteger(value.port) ||
    Number(value.port) < 1 ||
    Number(value.port) > 65_535
  ) {
    throw new AccountBridgeError("runtime-readiness-invalid");
  }
  return `http://127.0.0.1:${Number(value.port)}`;
}

async function spawnProductionSidecar(
  input: AccountBridgeSpawnInput,
): Promise<AccountBridgeChild> {
  const command = accountBridgeSpawnCommand(
    input.binaryPath,
    input.configPath,
    input.platform,
    process.env,
  );
  const child = spawn(command.file, command.args, {
    env: input.env,
    stdio: accountBridgeStdio(input.platform),
    windowsHide: true,
    detached: command.detached,
  });
  if (!child.pid) throw new AccountBridgeError("runtime-spawn-failed");
  const pipe = input.platform === "win32" ? child.stdin : child.stdio[3];
  if (!pipe || typeof (pipe as NodeJS.WritableStream).write !== "function") {
    child.kill();
    throw new AccountBridgeError("bootstrap-pipe-unavailable");
  }
  const bootstrapCopy = Buffer.from(input.bootstrap);
  await new Promise<void>((resolveWrite, reject) => {
    const writable = pipe as NodeJS.WritableStream;
    writable.once("error", reject);
    writable.write(bootstrapCopy, error => {
      bootstrapCopy.fill(0);
      if (error) reject(error);
      else {
        writable.end();
        resolveWrite();
      }
    });
  }).catch(() => {
    bootstrapCopy.fill(0);
    child.kill();
    throw new AccountBridgeError("bootstrap-write-failed");
  });
  return childProcessAdapter(
    child,
    input.platform,
    input.expectedProtocolVersion,
  );
}

export interface AccountBridgeSpawnCommand {
  file: string;
  args: string[];
  detached: boolean;
}

/**
 * Resolve the sidecar's process-tree owner.
 *
 * Unix gives the sidecar its own process group so TERM/KILL can target the
 * sidecar and its confirmed macOS plugin-host descendant. Windows must use
 * the native race-free Job Object wrapper; direct `ChildProcess.kill`
 * terminates only one PID and cannot prove descendant teardown.
 */
export function accountBridgeSpawnCommand(
  binaryPath: string,
  configPath: string,
  platform: NodeJS.Platform,
  hostEnvironment: NodeJS.ProcessEnv,
): AccountBridgeSpawnCommand {
  if (platform !== "win32") {
    return {
      file: binaryPath,
      args: ["-config", configPath],
      detached: true,
    };
  }
  const helper = hostEnvironment.CRABCODE_PROCESS_TREE_EXECUTABLE;
  if (
    typeof helper !== "string" ||
    helper.length === 0 ||
    !pathWin32.isAbsolute(helper)
  ) {
    throw new AccountBridgeError(
      "runtime-process-tree-helper-unavailable",
    );
  }
  return {
    file: helper,
    args: ["process-tree-exec", "--", binaryPath, "-config", configPath],
    detached: false,
  };
}

export const FACADE_ERROR_BODY_LIMIT_BYTES = 4096;
const FACADE_ERROR_DETAIL_RE = /^[a-z0-9_:-]{1,64}$/;

export async function readBoundedFacadeErrorDetail(
  response: Response,
): Promise<string | null> {
  try {
    const reader = response.body?.getReader();
    if (!reader) return null;
    const chunks: Uint8Array[] = [];
    let total = 0;
    while (total <= FACADE_ERROR_BODY_LIMIT_BYTES) {
      const { done, value } = await reader.read();
      if (done) break;
      if (value) {
        chunks.push(value);
        total += value.byteLength;
      }
    }
    void reader.cancel().catch(() => {});
    if (total > FACADE_ERROR_BODY_LIMIT_BYTES) return null;
    const parsed: unknown = JSON.parse(
      new TextDecoder().decode(concatUint8Arrays(chunks)),
    );
    if (!isRecord(parsed) || typeof parsed.error !== "string") return null;
    return FACADE_ERROR_DETAIL_RE.test(parsed.error) ? parsed.error : null;
  } catch {
    return null;
  }
}

function concatUint8Arrays(chunks: Uint8Array[]): Uint8Array {
  const merged = new Uint8Array(
    chunks.reduce((sum, chunk) => sum + chunk.byteLength, 0),
  );
  let offset = 0;
  for (const chunk of chunks) {
    merged.set(chunk, offset);
    offset += chunk.byteLength;
  }
  return merged;
}

async function defaultHTTP(input: {
  url: string;
  method: "GET" | "POST" | "DELETE";
  bearer: string | null;
  body?: unknown;
}): Promise<unknown> {
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), HTTP_TIMEOUT_MS);
  try {
    const response = await fetch(input.url, {
      method: input.method,
      signal: controller.signal,
      headers: {
        Accept: "application/json",
        ...(input.bearer ? { Authorization: `Bearer ${input.bearer}` } : {}),
        ...(input.body === undefined ? {} : { "Content-Type": "application/json" }),
      },
      ...(input.body === undefined ? {} : { body: JSON.stringify(input.body) }),
    });
    if (!response.ok) {
      const detail = (await readBoundedFacadeErrorDetail(response))
        ?.replace(/[_:]+/g, "-")
        .replace(/^-+|-+$/g, "");
      throw new AccountBridgeError(
        detail
          ? `facade-http-${response.status}-${detail}`
          : `facade-http-${response.status}`,
      );
    }
    return await response.json();
  } catch (error) {
    if (error instanceof AccountBridgeError) throw error;
    throw new AccountBridgeError("facade-unavailable");
  } finally {
    clearTimeout(timer);
  }
}

function injectedEligibilityTrustRoot(): string {
  let value: unknown;
  try {
    value =
      typeof ACCOUNT_BRIDGE_ELIGIBILITY_PUBLIC_KEY_BASE64URL === "undefined"
        ? undefined
        : ACCOUNT_BRIDGE_ELIGIBILITY_PUBLIC_KEY_BASE64URL;
  } catch {
    value = undefined;
  }
  const root = injectedString(value, "eligibility-trust-root-unavailable");
  strictBase64URL(root, 32).fill(0);
  return root;
}

function injectedConnectorPolicyTrustRoot(): string {
  let value: unknown;
  try {
    value =
      typeof ACCOUNT_BRIDGE_CONNECTOR_POLICY_PUBLIC_KEY_BASE64URL === "undefined"
        ? undefined
        : ACCOUNT_BRIDGE_CONNECTOR_POLICY_PUBLIC_KEY_BASE64URL;
  } catch {
    value = undefined;
  }
  const root = injectedString(
    value,
    "connector-policy-trust-root-unavailable",
  );
  strictBase64URL(root, 32).fill(0);
  return root;
}

function injectedArtifactTrustRoot(): string {
  let value: unknown;
  try {
    value =
      typeof ACCOUNT_BRIDGE_ARTIFACT_PUBLIC_KEY_BASE64URL === "undefined"
        ? undefined
        : ACCOUNT_BRIDGE_ARTIFACT_PUBLIC_KEY_BASE64URL;
  } catch {
    value = undefined;
  }
  const root = injectedString(value, "artifact-trust-root-unavailable");
  strictBase64URL(root, 32).fill(0);
  return root;
}

function injectedControlPlaneEndpoint(): string {
  let value: unknown;
  try {
    value =
      typeof ACCOUNT_BRIDGE_CONTROL_PLANE_ENDPOINT === "undefined"
        ? undefined
        : ACCOUNT_BRIDGE_CONTROL_PLANE_ENDPOINT;
  } catch {
    value = undefined;
  }
  const raw = injectedString(value, "eligibility-endpoint-unavailable");
  let endpoint: URL;
  try {
    endpoint = new URL(raw);
  } catch {
    throw new AccountBridgeError("eligibility-endpoint-invalid");
  }
  const host = endpoint.hostname.toLowerCase();
  if (
    endpoint.protocol !== "https:" ||
    endpoint.username !== "" ||
    endpoint.password !== "" ||
    endpoint.search !== "" ||
    endpoint.hash !== "" ||
    endpoint.pathname === "/" ||
    endpoint.toString() !== raw ||
    !(
      host === "acosmi.com" ||
      host.endsWith(".acosmi.com")
    )
  ) {
    throw new AccountBridgeError("eligibility-endpoint-invalid");
  }
  return endpoint.toString();
}

async function readBoundedControlPlaneResponse(
  response: Response,
): Promise<AccountBridgeControlPlaneEnvelope> {
  const contentType = response.headers.get("content-type")?.toLowerCase() ?? "";
  const declaredLength = Number(response.headers.get("content-length") ?? "0");
  if (
    !contentType.startsWith("application/json") ||
    (Number.isFinite(declaredLength) && declaredLength > CONTROL_PLANE_MAX_BYTES)
  ) {
    throw new AccountBridgeError("eligibility-response-invalid");
  }
  const reader = response.body?.getReader();
  if (!reader) throw new AccountBridgeError("eligibility-response-invalid");
  const chunks: Uint8Array[] = [];
  let length = 0;
  try {
    while (true) {
      const { done, value } = await reader.read();
      if (done) break;
      length += value.byteLength;
      if (length > CONTROL_PLANE_MAX_BYTES) {
        await reader.cancel();
        throw new AccountBridgeError("eligibility-response-invalid");
      }
      chunks.push(value);
    }
    const raw = Buffer.concat(chunks.map(chunk => Buffer.from(chunk)), length);
    try {
      const decoded = JSON.parse(raw.toString("utf8")) as unknown;
      return parseAccountBridgeControlPlaneEnvelope(decoded);
    } catch (error) {
      if (error instanceof AccountBridgeError) throw error;
      throw new AccountBridgeError("eligibility-response-invalid");
    } finally {
      raw.fill(0);
    }
  } finally {
    for (const chunk of chunks) chunk.fill(0);
  }
}

async function fetchProductionControlPlane(
  controlPlaneRequest: AccountBridgeControlPlaneRequest,
): Promise<AccountBridgeControlPlaneEnvelope> {
  const endpoint = injectedControlPlaneEndpoint();
  const requestBinding = exactControlPlaneClient(
    controlPlaneRequest,
    "control-plane-request-invalid",
  );
  const [{ getAuthHeaders }, { checkAndRefreshOAuthTokenIfNeeded }] =
    await Promise.all([
      import("../../utils/http.js"),
      import("../../utils/auth.js"),
    ]);
  await checkAndRefreshOAuthTokenIfNeeded();
  const request = async (forceRefresh: boolean): Promise<Response> => {
    if (forceRefresh) await checkAndRefreshOAuthTokenIfNeeded(0, true);
    const auth = getAuthHeaders();
    if (auth.error || Object.keys(auth.headers).length !== 1) {
      throw new AccountBridgeError("eligibility-auth-unavailable");
    }
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), HTTP_TIMEOUT_MS);
    try {
      return await fetch(endpoint, {
        method: "GET",
        headers: {
          ...auth.headers,
          Accept: "application/json",
          "X-CrabCode-Release": requestBinding.crabCodeRelease,
          "X-Account-Bridge-Component":
            requestBinding.accountBridgeComponentVersion,
          "X-Account-Bridge-Protocol": String(
            requestBinding.accountBridgeProtocolVersion,
          ),
          "X-Account-Bridge-Request-Nonce": requestBinding.requestNonce,
        },
        signal: controller.signal,
        redirect: "error",
        cache: "no-store",
        credentials: "omit",
      });
    } catch {
      throw new AccountBridgeError("eligibility-service-unavailable");
    } finally {
      clearTimeout(timer);
    }
  };
  let response = await request(false);
  if (response.status === 401) response = await request(true);
  if (!response.ok) {
    throw new AccountBridgeError(
      response.status === 401 || response.status === 403
        ? "eligibility-auth-denied"
        : "eligibility-service-unavailable",
    );
  }
  return readBoundedControlPlaneResponse(response);
}

/**
 * Production consent reader: the persisted
 * `userSettings.accountBridge.eligibilityConsent` key via the same settings
 * truth source used by the worker's config-read resolvers
 * (`src/utils/settings/settings.ts`).
 * The module is resolved lazily — a static import would crash raw-source
 * worker tests (MACRO graph) and the source-free bundle only rewrites
 * dynamic imports. Consent is a security gate, so prefer
 * `refreshSettingsForSource` (fresh disk read; a withdrawal written by
 * another CrabCode process must not be masked by a watcher-lagged cache).
 * Every failure path returns `null` = fail-closed (consent-required).
 */
async function readProductionEligibilityConsent(): Promise<AccountBridgeEligibilityConsent | null> {
  try {
    const settings = await import("../../utils/settings/settings.js");
    const userSettings =
      settings.refreshSettingsForSource?.("userSettings") ??
      settings.getSettingsForSource("userSettings");
    if (!isRecord(userSettings)) return null;
    const bridge = userSettings["accountBridge"];
    if (!isRecord(bridge)) return null;
    const consent = bridge["eligibilityConsent"];
    if (!isRecord(consent)) return null;
    const grantedAtIso = consent["grantedAtIso"];
    const noticeVersion = consent["noticeVersion"];
    if (
      typeof grantedAtIso !== "string" ||
      grantedAtIso.trim().length === 0 ||
      typeof noticeVersion !== "number" ||
      !Number.isSafeInteger(noticeVersion)
    ) {
      return null;
    }
    return { grantedAtIso, noticeVersion };
  } catch {
    return null;
  }
}

function productionNetworkChangeSubscription(listener: () => void): () => void {
  const signature = (): string => {
    const rows: string[] = [];
    for (const [name, addresses] of Object.entries(networkInterfaces())) {
      for (const address of addresses ?? []) {
        if (address.internal) continue;
        rows.push(
          `${name}:${address.family}:${address.address}:${address.netmask}:${address.scopeid ?? 0}`,
        );
      }
    }
    return rows.sort().join("|");
  };
  let previous = signature();
  const poll = setInterval(() => {
    const next = signature();
    if (next === previous) return;
    previous = next;
    listener();
  }, NETWORK_SIGNATURE_POLL_MS);
  poll.unref?.();
  const target = globalThis as unknown as {
    addEventListener?: (name: string, handler: () => void) => void;
    removeEventListener?: (name: string, handler: () => void) => void;
  };
  const eventHandler = (): void => listener();
  target.addEventListener?.("online", eventHandler);
  target.addEventListener?.("offline", eventHandler);
  return () => {
    clearInterval(poll);
    target.removeEventListener?.("online", eventHandler);
    target.removeEventListener?.("offline", eventHandler);
  };
}

function productionDeps(): AccountBridgeManagerDeps {
  return {
    now: Date.now,
    sleep: ms => new Promise(resolveSleep => setTimeout(resolveSleep, ms)),
    randomBytes: size => randomBytes(size),
    readEligibilityConsent: readProductionEligibilityConsent,
    releaseIdentity: () => ({
      crabCodeRelease: injectedCrabCodeReleaseVersion(),
      accountBridgeComponentVersion: injectedComponentVersion(),
      accountBridgeProtocolVersion: injectedProtocolVersion(),
    }),
    fetchControlPlane: fetchProductionControlPlane,
    verifyGrant: (grant, request, now) =>
      verifyEligibilityGrantWithKey(
        grant,
        injectedEligibilityTrustRoot(),
        request,
        now,
      ),
    verifyConnectorDirectory: (directory, request, now) =>
      verifyConnectorPolicyDirectoryWithKey(
        directory,
        injectedConnectorPolicyTrustRoot(),
        request,
        now,
      ),
    resolveAndVerifyArtifact: async () => {
      const paths = packagedArtifactPaths();
      return verifyPackagedArtifact({
        ...paths,
        expectedComponentVersion: injectedComponentVersion(),
        expectedPlatform: platformTag(),
        expectedProtocolVersion: injectedProtocolVersion(),
        artifactPublicKeyBase64URL: injectedArtifactTrustRoot(),
        eligibilityPublicKeyBase64URL: injectedEligibilityTrustRoot(),
      });
    },
    loadOrCreateMasterKey: loadOrCreateAccountBridgeMasterKey,
    acquireRuntimeLock: acquireProductionLock,
    createRuntimeFiles: createProductionRuntimeFiles,
    removeRuntimeFiles: runtimeDir => fs.rm(runtimeDir, { recursive: true, force: true }),
    spawnSidecar: spawnProductionSidecar,
    http: defaultHTTP,
    writeDiagnosticBundle: bundle => writeAccountBridgeDiagnosticBundle(bundle),
    platform: process.platform,
    subscribeNetworkChanges: productionNetworkChangeSubscription,
  };
}

let managerSingleton: AccountBridgeManager | null = null;

export function getAccountBridgeManager(): AccountBridgeManager {
  managerSingleton ??= new AccountBridgeManager();
  return managerSingleton;
}

export type AccountBridgeProcessShutdownResult =
  | { state: "not-created" }
  | { state: "stopped" };

export function beginAccountBridgeManagerProcessShutdown(): void {
  managerSingleton?.beginProcessShutdown();
}

/**
 * Worker-shutdown entry that never creates the singleton merely to stop it.
 * A created manager is cleared only after its sidecar, timers, subscription,
 * runtime files, and lock have all settled.
 */
export async function shutdownAccountBridgeManagerForProcess(): Promise<AccountBridgeProcessShutdownResult> {
  const manager = managerSingleton;
  if (!manager) return { state: "not-created" };
  const status = await manager.shutdownForProcess();
  if (status.state !== "stopped") {
    throw new AccountBridgeError("runtime-stop-incomplete");
  }
  if (managerSingleton === manager) managerSingleton = null;
  return { state: "stopped" };
}

/** Test-only singleton seam; production callers must never replace the manager. */
export function _setAccountBridgeManagerForTest(
  manager: AccountBridgeManager | null,
): void {
  managerSingleton = manager;
}
