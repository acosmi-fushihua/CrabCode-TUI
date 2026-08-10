import { afterEach, describe, expect, test } from "bun:test";
import {
  createHash,
  generateKeyPairSync,
  sign,
  type KeyObject,
} from "node:crypto";
import * as fs from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import {
  _setAccountBridgeManagerForTest,
  AccountBridgeError,
  AccountBridgeManager,
  accountBridgeAutomaticRestartDelay,
  accountBridgeSpawnCommand,
  accountBridgeStdio,
  parseAccountBridgeControlPlaneEnvelope,
  parseAccountBridgeReadyLine,
  sanitizedSidecarEnv,
  shutdownAccountBridgeManagerForProcess,
  verifyConnectorPolicyDirectoryWithKey,
  verifyEligibilityGrantWithKey,
  verifyPackagedArtifact,
  writeAccountBridgeDiagnosticBundle,
  type AccountBridgeChild,
  type AccountBridgeControlPlaneRequest,
  type AccountBridgeEligibilityConsent,
  type AccountBridgeLocalDiagnosticBundle,
  type AccountBridgeManagerDeps,
  type AccountBridgeSidecarForensics,
  type AccountBridgeSpawnInput,
  type SignedEligibilityGrant,
} from "../../src/services/accountBridge/runtimeManager.js";
import {
  AccountBridgeMasterKeyError,
  createLinuxSecretServiceBackend,
  createWindowsDPAPIBackend,
  loadOrCreateAccountBridgeMasterKey,
  WINDOWS_DPAPI_SCRIPT,
  type AccountBridgeMasterKeyBackend,
  type AccountBridgeMasterKeyDeps,
  type AccountBridgeSecureCommand,
} from "../../src/services/accountBridge/masterKey.js";
import {
  acquireDirectAccountBridgeTurnAccess,
  type DirectAccountBridgeTurnError,
} from "../../src/services/accountBridge/directTurnAccess.js";

const ROUTE_A = "A".repeat(43);
const ROUTE_B = "B".repeat(43);
const NOW = Date.parse("2026-07-13T12:00:00.000Z");
const RELEASE_IDENTITY = {
  crabCodeRelease: "1.0.13",
  accountBridgeComponentVersion: "1.0.13-account-bridge.1",
  accountBridgeProtocolVersion: 1,
} as const;
const TEST_CONTROL_PLANE_REQUEST: AccountBridgeControlPlaneRequest = {
  requestNonce: Buffer.alloc(32, 0x51).toString("base64url"),
  ...RELEASE_IDENTITY,
};

function exactAllowedClientVersions(request: AccountBridgeControlPlaneRequest) {
  return {
    crabCodeRelease: {
      minimumInclusive: request.crabCodeRelease,
      maximumInclusive: request.crabCodeRelease,
    },
    accountBridgeComponentVersion: {
      minimumInclusive: request.accountBridgeComponentVersion,
      maximumInclusive: request.accountBridgeComponentVersion,
    },
    accountBridgeProtocolVersion: {
      minimumInclusive: request.accountBridgeProtocolVersion,
      maximumInclusive: request.accountBridgeProtocolVersion,
    },
  };
}

type Deferred<T> = {
  promise: Promise<T>;
  resolve(value: T): void;
};

function deferred<T>(): Deferred<T> {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>(done => {
    resolve = done;
  });
  return { promise, resolve };
}

function emptySidecarForensics(): AccountBridgeSidecarForensics {
  return {
    exitCode: null,
    exitSignal: null,
    spawnErrorCode: null,
    bannerLine: null,
    logLines: [],
    droppedLineCount: 0,
    readyLineRejected: false,
  };
}

function signedGrant(input: {
  countryCode?: string;
  regionAllowed?: boolean;
  issuedAt?: number;
  expiresAt?: number;
  extra?: Record<string, unknown>;
  client?: AccountBridgeControlPlaneRequest;
  allowedClientVersions?: ReturnType<typeof exactAllowedClientVersions>;
  signer?: { publicKey: KeyObject; privateKey: KeyObject };
} = {}): {
  grant: SignedEligibilityGrant;
  publicKey: string;
} {
  const { publicKey, privateKey } =
    input.signer ?? generateKeyPairSync("ed25519");
  const issuedAt = input.issuedAt ?? Math.floor(NOW / 1_000) - 30;
  const client = input.client ?? TEST_CONTROL_PLANE_REQUEST;
  const payload = {
    audience: "crabcode-account-bridge",
    version: "v1",
    client,
    allowedClientVersions:
      input.allowedClientVersions ?? exactAllowedClientVersions(client),
    policyVersion: "policy-test",
    issuedAt,
    expiresAt: input.expiresAt ?? issuedAt + 300,
    countryCode: input.countryCode ?? "US",
    regionAllowed: input.regionAllowed ?? true,
    connectors: ["openai", "anthropic", "google", "xai"],
    ...(input.extra ?? {}),
  };
  const raw = Buffer.from(JSON.stringify(payload));
  const spki = publicKey.export({ format: "der", type: "spki" });
  return {
    grant: {
      payload: raw.toString("base64url"),
      signature: sign(null, raw, privateKey).toString("base64url"),
    },
    publicKey: spki.subarray(spki.length - 32).toString("base64url"),
  };
}

function signedConnectorDirectory(
  enabled: boolean | readonly string[] = false,
  googleEnabled = false,
  time: { issuedAt?: number; expiresAt?: number } = {},
  mutate?: (payload: {
    audience: string;
    version: string;
    client: AccountBridgeControlPlaneRequest;
    allowedClientVersions: ReturnType<typeof exactAllowedClientVersions>;
    policyVersion: string;
    issuedAt: number;
    expiresAt: number;
    connectors: Array<Record<string, unknown>>;
  }) => void,
  client: AccountBridgeControlPlaneRequest = TEST_CONTROL_PLANE_REQUEST,
  signer: { publicKey: KeyObject; privateKey: KeyObject } =
    generateKeyPairSync("ed25519"),
) {
  const { publicKey, privateKey } = signer;
  const enabledConnectors = new Set<string>(
    Array.isArray(enabled) ? enabled : enabled ? ["openai"] : [],
  );
  if (googleEnabled) enabledConnectors.add("google");
  const issuedAt = time.issuedAt ?? Math.floor(NOW / 1_000) - 30;
  const displayNames: Record<string, string> = {
    openai: "OpenAI",
    anthropic: "Anthropic",
    google: "Google",
    xai: "xAI",
  };
  const payload = {
    audience: "crabcode-account-bridge-connectors",
    version: "v1",
    client,
    allowedClientVersions: exactAllowedClientVersions(client),
    policyVersion: "connector-policy-test",
    issuedAt,
    expiresAt: time.expiresAt ?? issuedAt + 300,
    connectors: ["openai", "anthropic", "google", "xai"].map(connectorId => ({
      connectorId,
      displayName: displayNames[connectorId]!,
      authMode: connectorId === "xai" ? "device-code" : "browser",
      featureEnabled: enabledConnectors.has(connectorId),
      termsStatus: enabledConnectors.has(connectorId)
        ? "signed-off"
        : "blocked",
      conformancePassed: enabledConnectors.has(connectorId),
      fixedArtifactVerified: enabledConnectors.has(connectorId),
    })),
  };
  mutate?.(payload);
  const raw = Buffer.from(JSON.stringify(payload));
  const spki = publicKey.export({ format: "der", type: "spki" });
  return {
    directory: {
      payload: raw.toString("base64url"),
      signature: sign(null, raw, privateKey).toString("base64url"),
    },
    publicKey: spki.subarray(spki.length - 32).toString("base64url"),
  };
}

const ALL_CONNECTOR_IDS = [
  "openai",
  "anthropic",
  "google",
  "xai",
  "qwen",
  "kimi",
  "zai",
] as const;

const V2_CONNECTOR_DISPLAY_NAMES: Record<string, string> = {
  openai: "OpenAI",
  anthropic: "Anthropic",
  google: "Google",
  xai: "xAI",
  qwen: "Qwen Code",
  kimi: "Kimi Code",
  zai: "Z Code",
};

const V2_CONNECTOR_REGION_POLICIES: Record<string, string> = {
  openai: "non-cn",
  anthropic: "non-cn",
  google: "non-cn",
  xai: "non-cn",
  qwen: "global",
  kimi: "global",
  zai: "global",
};

function v2DirectoryConnectors(): Array<Record<string, unknown>> {
  return ALL_CONNECTOR_IDS.map(connectorId => ({
    connectorId,
    displayName: V2_CONNECTOR_DISPLAY_NAMES[connectorId]!,
    authMode:
      connectorId === "openai" ||
      connectorId === "anthropic" ||
      connectorId === "google"
        ? "browser"
        : "device-code",
    featureEnabled: true,
    termsStatus: "signed-off",
    conformancePassed: true,
    fixedArtifactVerified: true,
    regionPolicy: V2_CONNECTOR_REGION_POLICIES[connectorId]!,
  }));
}

function route(routeId = ROUTE_A) {
  return {
    routeId,
    accountId: "opaque-account",
    connectorId: "openai",
    modelId: "model-exact",
    displayName: null,
    connectorLabel: "OpenAI",
    accountLabel: "Account AAAAAAAA",
    chatRuntimeSupported: true,
    supportsTools: true,
    supportsThinking: null,
    supportsAdaptiveThinking: null,
    supportsEffort: null,
    supportsMaxEffort: null,
    supportsVision: null,
    supportsJsonMode: null,
    supportedThinkingModes: [],
    defaultThinkingMode: null,
    contextWindow: null,
    maxOutputTokens: null,
  };
}

function connector(enabled = false) {
  return {
    connectorId: "openai",
    displayName: "OpenAI",
    authMode: "browser" as const,
    enabled,
    disabledReasonCode: enabled ? null : "terms_not_signed_off",
    termsStatus: (enabled ? "signed-off" : "blocked") as
      | "signed-off"
      | "blocked",
  };
}

function usage(
  state: "available" | "cooldown" | "exhausted" | "unknown" = "available",
  routeId = ROUTE_A,
  accountId = "opaque-account",
) {
  return {
    routeId,
    accountId,
    state,
    remainingPercent: state === "available" ? 50 : null,
    limitingWindowLabel: state === "available" ? "daily" : null,
    resetsAt: null,
    windows: [],
    observedAt: "2026-07-13T12:00:00.000Z",
  };
}

function fakeManagerDeps(options: {
  enabled?: boolean;
  countryCode?: string;
  googleDirectoryEnabled?: boolean;
  routeResponse?: unknown;
  accountResponse?: unknown;
  connectorResponse?: unknown;
  usageResponse?: unknown;
  now?: () => number;
  connectorExpiresAt?: number;
  enabledConnectors?: () => readonly string[];
  /** Control-plane generation minted by fetchControlPlane (default v1). */
  generation?: "v1" | "v2";
  /** Directory generation override — used for grant↔directory mismatch. */
  directoryGeneration?: "v1" | "v2";
  /** v2 grant `connectors` payload (default: all seven). */
  grantConnectors?: readonly string[];
  regionAllowed?: boolean;
  /** Detection-consent stub; default = a current granted consent. */
  readEligibilityConsent?: () => AccountBridgeEligibilityConsent | null;
} = {}): {
  deps: AccountBridgeManagerDeps;
  children: AccountBridgeChild[];
  exits: Array<Deferred<number | null>>;
  spawns: AccountBridgeSpawnInput[];
  bootstrapCopies: Buffer[];
  configs: Array<{ managementKey: string; inferenceKey: string }>;
  diagnosticBundles: AccountBridgeLocalDiagnosticBundle[];
  httpURLs: string[];
  removedRuntimeDirs: string[];
  stopCalls: { terminate: number; kill: number; released: number };
} {
  const eligibilitySigner = generateKeyPairSync("ed25519");
  const policySigner = generateKeyPairSync("ed25519");
  const eligibilitySpki = eligibilitySigner.publicKey.export({
    format: "der",
    type: "spki",
  });
  const policySpki = policySigner.publicKey.export({
    format: "der",
    type: "spki",
  });
  const eligibilityPublicKey = eligibilitySpki
    .subarray(eligibilitySpki.length - 32)
    .toString("base64url");
  const policyPublicKey = policySpki
    .subarray(policySpki.length - 32)
    .toString("base64url");
  const stopCalls = { terminate: 0, kill: 0, released: 0 };
  const children: AccountBridgeChild[] = [];
  const exits: Array<Deferred<number | null>> = [];
  const spawns: AccountBridgeSpawnInput[] = [];
  const bootstrapCopies: Buffer[] = [];
  const configs: Array<{ managementKey: string; inferenceKey: string }> = [];
  const diagnosticBundles: AccountBridgeLocalDiagnosticBundle[] = [];
  const httpURLs: string[] = [];
  const removedRuntimeDirs: string[] = [];
  let randomCall = 0;
  const generation = options.generation ?? "v1";
  const directoryGeneration = options.directoryGeneration ?? generation;
  const deps: AccountBridgeManagerDeps = {
    now: options.now ?? (() => NOW),
    sleep: async () => {},
    randomBytes: size => new Uint8Array(size).fill(++randomCall + 7),
    readEligibilityConsent:
      options.readEligibilityConsent ??
      (() => ({
        grantedAtIso: "2026-07-13T00:00:00.000Z",
        noticeVersion: 1,
      })),
    releaseIdentity: () => RELEASE_IDENTITY,
    fetchControlPlane: async request => ({
      eligibilityGrant: signedGrant({
        client: request,
        signer: eligibilitySigner,
        countryCode: options.countryCode,
        regionAllowed: options.regionAllowed,
        ...(generation === "v2"
          ? {
              extra: {
                version: "v2",
                connectors: [
                  ...(options.grantConnectors ?? ALL_CONNECTOR_IDS),
                ],
              },
            }
          : {}),
      }).grant,
      connectorDirectory: signedConnectorDirectory(
        options.enabledConnectors?.() ?? options.enabled === true,
        options.googleDirectoryEnabled === true,
        { expiresAt: options.connectorExpiresAt },
        directoryGeneration === "v2"
          ? payload => {
              payload.version = "v2";
              payload.connectors = v2DirectoryConnectors();
            }
          : undefined,
        request,
        policySigner,
      ).directory,
    }),
    verifyGrant: (grant, request, now) =>
      verifyEligibilityGrantWithKey(
        grant,
        eligibilityPublicKey,
        request,
        now,
      ),
    verifyConnectorDirectory: (directory, request, now) =>
      verifyConnectorPolicyDirectoryWithKey(
        directory,
        policyPublicKey,
        request,
        now,
      ),
    resolveAndVerifyArtifact: async () => ({
      binaryPath: "/packaged/bin/oauthapi-llm",
      metadataDir: "/packaged/bin/account-bridge",
      componentVersion: "1.0.13-account-bridge.1",
      protocolVersion: 1,
      fixedPlugins: [],
    }),
    loadOrCreateMasterKey: async () => new Uint8Array(32).fill(3),
    acquireRuntimeLock: async () => ({
      async release() {
        stopCalls.released += 1;
      },
    }),
    createRuntimeFiles: async input => {
      configs.push(input);
      return { configPath: "/private/config.json", runtimeDir: "/private/runtime" };
    },
    removeRuntimeFiles: async runtimeDir => {
      removedRuntimeDirs.push(runtimeDir);
    },
    spawnSidecar: async input => {
      spawns.push(input);
      bootstrapCopies.push(Buffer.from(input.bootstrap));
      const exit = deferred<number | null>();
      const child: AccountBridgeChild = {
        pid: 4321 + children.length,
        endpoint: Promise.resolve(`http://127.0.0.1:${43123 + children.length}`),
        exited: exit.promise,
        terminate() {
          stopCalls.terminate += 1;
        },
        kill() {
          stopCalls.kill += 1;
          exit.resolve(null);
        },
        forensics: emptySidecarForensics,
      };
      exits.push(exit);
      children.push(child);
      return child;
    },
    http: async input => {
      httpURLs.push(input.url);
      if (input.url.endsWith("/healthz")) return { status: "ok" };
      if (input.url.includes("/connectors")) {
        return (
          options.connectorResponse ?? {
            connectors: [connector(options.enabled === true)],
          }
        );
      }
      if (input.url.includes("/routes")) {
        return options.routeResponse ?? { routes: [route()] };
      }
      if (input.url.includes("/accounts")) {
        return options.accountResponse ?? {
          accounts: [
            {
              accountId: "opaque-account",
              connectorId: "openai",
              displayLabel: "Account AAAAAAAA",
              status: "ready",
              connectedAt: "2026-07-13T00:00:00.000Z",
              lastUsedAt: null,
              cooldownUntil: null,
            },
          ],
        };
      }
      if (input.url.includes("/usage")) {
        return options.usageResponse ?? { snapshots: [] };
      }
      throw new Error("unexpected fake HTTP request");
    },
    writeDiagnosticBundle: async bundle => {
      diagnosticBundles.push(bundle);
    },
    platform: "darwin",
  };
  return {
    deps,
    children,
    exits,
    spawns,
    bootstrapCopies,
    configs,
    diagnosticBundles,
    httpURLs,
    removedRuntimeDirs,
    stopCalls,
  };
}

afterEach(() => {
  _setAccountBridgeManagerForTest(null);
});

describe("Account Bridge signed eligibility", () => {
  test("accepts only exact <=5m signed non-CN grants", () => {
    const signed = signedGrant();
    const verified = verifyEligibilityGrantWithKey(
      signed.grant,
      signed.publicKey,
      TEST_CONTROL_PLANE_REQUEST,
      NOW,
    );
    expect(verified.view.state).toBe("allowed");
    expect(verified.view.countryCode).toBe("US");
    expect(JSON.stringify(verified.view)).not.toContain("signature");
    expect(JSON.stringify(verified.view)).not.toContain("payload");
  });

  test("recognizes signed CN as blocked and rejects unknown/tampered/overlong grants", () => {
    const cn = signedGrant({ countryCode: "CN" });
    expect(
      verifyEligibilityGrantWithKey(
        cn.grant,
        cn.publicKey,
        TEST_CONTROL_PLANE_REQUEST,
        NOW,
      ).view.state,
    ).toBe("blocked-cn");

    const unknown = signedGrant({ countryCode: "ZZ" });
    expect(() =>
      verifyEligibilityGrantWithKey(
        unknown.grant,
        unknown.publicKey,
        TEST_CONTROL_PLANE_REQUEST,
        NOW,
      ),
    ).toThrow("unknown-region");

    const overlong = signedGrant({
      expiresAt: Math.floor(NOW / 1_000) + 301,
      issuedAt: Math.floor(NOW / 1_000),
    });
    expect(() =>
      verifyEligibilityGrantWithKey(
        overlong.grant,
        overlong.publicKey,
        TEST_CONTROL_PLANE_REQUEST,
        NOW,
      ),
    ).toThrow("invalid-grant-policy");

    const tampered = signedGrant();
    tampered.grant.payload = Buffer.from("{}").toString("base64url");
    expect(() =>
      verifyEligibilityGrantWithKey(
        tampered.grant,
        tampered.publicKey,
        TEST_CONTROL_PLANE_REQUEST,
        NOW,
      ),
    ).toThrow("invalid-signature");
  });

  test("binds both signed responses to the exact request nonce and release identity", () => {
    const replayedGrant = signedGrant({
      client: {
        ...TEST_CONTROL_PLANE_REQUEST,
        requestNonce: Buffer.alloc(32, 0x52).toString("base64url"),
      },
    });
    expect(() =>
      verifyEligibilityGrantWithKey(
        replayedGrant.grant,
        replayedGrant.publicKey,
        TEST_CONTROL_PLANE_REQUEST,
        NOW,
      ),
    ).toThrow("control-plane-response-replay");

    const mismatchedDirectory = signedConnectorDirectory(
      false,
      false,
      {},
      undefined,
      {
        ...TEST_CONTROL_PLANE_REQUEST,
        accountBridgeComponentVersion: "1.0.14-account-bridge.1",
      },
    );
    expect(() =>
      verifyConnectorPolicyDirectoryWithKey(
        mismatchedDirectory.directory,
        mismatchedDirectory.publicKey,
        TEST_CONTROL_PLANE_REQUEST,
        NOW,
      ),
    ).toThrow("control-plane-response-version-mismatch");

    const unsupportedProtocol = signedGrant({
      allowedClientVersions: {
        ...exactAllowedClientVersions(TEST_CONTROL_PLANE_REQUEST),
        accountBridgeProtocolVersion: {
          minimumInclusive: 2,
          maximumInclusive: 3,
        },
      },
    });
    expect(() =>
      verifyEligibilityGrantWithKey(
        unsupportedProtocol.grant,
        unsupportedProtocol.publicKey,
        TEST_CONTROL_PLANE_REQUEST,
        NOW,
      ),
    ).toThrow("control-plane-client-version-unsupported");
  });

  test("accepts signed connector presentation metadata and rejects tampering", () => {
    const signed = signedConnectorDirectory();
    const verified = verifyConnectorPolicyDirectoryWithKey(
      signed.directory,
      signed.publicKey,
      TEST_CONTROL_PLANE_REQUEST,
      NOW,
    );
    expect(verified.payload.policyVersion).toBe("connector-policy-test");
    expect(verified.policies.map(policy => ({
      connectorId: policy.connectorId,
      displayName: policy.displayName,
      authMode: policy.authMode,
    }))).toEqual([
      { connectorId: "openai", displayName: "OpenAI", authMode: "browser" },
      { connectorId: "anthropic", displayName: "Anthropic", authMode: "browser" },
      { connectorId: "google", displayName: "Google", authMode: "browser" },
      { connectorId: "xai", displayName: "xAI", authMode: "device-code" },
    ]);

    const payload = JSON.parse(
      Buffer.from(signed.directory.payload, "base64url").toString("utf8"),
    );
    payload.connectors[0].displayName = "Tampered label";
    signed.directory.payload = Buffer.from(JSON.stringify(payload)).toString(
      "base64url",
    );
    expect(() =>
      verifyConnectorPolicyDirectoryWithKey(
        signed.directory,
        signed.publicKey,
        TEST_CONTROL_PLANE_REQUEST,
        NOW,
      ),
    ).toThrow("connector-policy-signature-invalid");
  });

  test("requires the exact four-entry connector directory and strict metadata DTO", () => {
    const invalidDirectories: Array<{
      name: string;
      mutate(payload: { connectors: Array<Record<string, unknown>> }): void;
    }> = [
      {
        name: "missing connector",
        mutate: payload => { payload.connectors.pop(); },
      },
      {
        name: "duplicate connector",
        mutate: payload => {
          payload.connectors[3] = { ...payload.connectors[0] };
        },
      },
      {
        name: "missing displayName",
        mutate: payload => { delete payload.connectors[0]!.displayName; },
      },
      {
        name: "blank displayName",
        mutate: payload => { payload.connectors[0]!.displayName = ""; },
      },
      {
        name: "overlong displayName",
        mutate: payload => { payload.connectors[0]!.displayName = "A".repeat(81); },
      },
      {
        name: "control displayName",
        mutate: payload => { payload.connectors[0]!.displayName = "Open\nAI"; },
      },
      {
        name: "format-control displayName",
        mutate: payload => { payload.connectors[0]!.displayName = "Open\u202eAI"; },
      },
      {
        name: "invalid authMode",
        mutate: payload => { payload.connectors[0]!.authMode = "redirect"; },
      },
      {
        name: "unknown connector field",
        mutate: payload => { payload.connectors[0]!.backend = "codex"; },
      },
    ];

    for (const testCase of invalidDirectories) {
      const signed = signedConnectorDirectory(
        false,
        false,
        {},
        payload => testCase.mutate(payload),
      );
      expect(
        () => verifyConnectorPolicyDirectoryWithKey(
          signed.directory,
          signed.publicKey,
          TEST_CONTROL_PLANE_REQUEST,
          NOW,
        ),
        testCase.name,
      ).toThrow();
    }
  });

  test("keeps eligibility and connector-directory trust domains non-reusable", () => {
    const eligibility = signedGrant();
    const policies = signedConnectorDirectory();
    expect(() =>
      verifyConnectorPolicyDirectoryWithKey(
        policies.directory,
        eligibility.publicKey,
        TEST_CONTROL_PLANE_REQUEST,
        NOW,
      ),
    ).toThrow("connector-policy-signature-invalid");
    expect(() =>
      verifyEligibilityGrantWithKey(
        eligibility.grant,
        policies.publicKey,
        TEST_CONTROL_PLANE_REQUEST,
        NOW,
      ),
    ).toThrow("invalid-signature");
    expect(() =>
      parseAccountBridgeControlPlaneEnvelope({
        eligibilityGrant: eligibility.grant,
        connectorDirectory: policies.directory,
        publicKey: policies.publicKey,
      }),
    ).toThrow("eligibility-response-invalid");
  });
});

describe("Account Bridge lifecycle", () => {
  test("projects all four signed connectors as disabled in blocked regions without starting the sidecar", async () => {
    const fixture = fakeManagerDeps({
      countryCode: "CN",
      enabledConnectors: () => ["openai", "anthropic", "google", "xai"],
    });
    const manager = new AccountBridgeManager(fixture.deps);

    expect(await manager.eligibilityRead(false)).toMatchObject({
      state: "blocked-cn",
      countryCode: "CN",
    });
    // W-ACCOUNT-BRIDGE-REGION-CONNECTORS: under a CN egress the legacy
    // 'non-cn' rows now carry the honest region reason instead of the old
    // aggregate eligibility_denied. Enablement is unchanged (all disabled).
    expect(await manager.connectorList()).toEqual([
      {
        connectorId: "openai",
        displayName: "OpenAI",
        authMode: "browser",
        enabled: false,
        disabledReasonCode: "connector_region_blocked",
        termsStatus: "signed-off",
      },
      {
        connectorId: "anthropic",
        displayName: "Anthropic",
        authMode: "browser",
        enabled: false,
        disabledReasonCode: "connector_region_blocked",
        termsStatus: "signed-off",
      },
      {
        connectorId: "google",
        displayName: "Google",
        authMode: "browser",
        enabled: false,
        disabledReasonCode: "connector_region_blocked",
        termsStatus: "signed-off",
      },
      {
        connectorId: "xai",
        displayName: "xAI",
        authMode: "device-code",
        enabled: false,
        disabledReasonCode: "connector_region_blocked",
        termsStatus: "signed-off",
      },
    ]);
    expect(fixture.spawns).toHaveLength(0);
    expect(fixture.httpURLs).toHaveLength(0);
  });

  test("falls back to the static shipped-connector catalog when the control plane is unreachable", async () => {
    const fixture = fakeManagerDeps();
    fixture.deps.fetchControlPlane = async () => {
      throw new AccountBridgeError("eligibility-auth-denied");
    };
    const manager = new AccountBridgeManager(fixture.deps);
    expect(await manager.eligibilityRead(false)).toMatchObject({
      state: "unavailable",
      reasonCode: "eligibility-auth-denied",
    });
    // The page must never collapse into "no connector directory": the seven
    // shipped connectors stay visible with an honest disabled reason while
    // every action remains fail-closed.
    const connectors = await manager.connectorList();
    expect(connectors.map(item => item.connectorId)).toEqual([
      "openai",
      "anthropic",
      "google",
      "xai",
      "qwen",
      "kimi",
      "zai",
    ]);
    expect(connectors.find(item => item.connectorId === "xai")?.authMode).toBe(
      "device-code",
    );
    for (const item of connectors) {
      expect(item.enabled).toBeFalse();
      expect(item.disabledReasonCode).toBe("eligibility_denied");
    }
    expect(fixture.spawns).toHaveLength(0);
  });

  test("a failed runtime keeps its diagnostic code across later successful eligibility refreshes", async () => {
    const fixture = fakeManagerDeps();
    fixture.deps.resolveAndVerifyArtifact = async () => {
      throw new AccountBridgeError("provenance-invalid");
    };
    const manager = new AccountBridgeManager(fixture.deps);
    await expect(manager.ensure()).rejects.toMatchObject({
      code: "provenance-invalid",
    });
    expect(manager.status()).toMatchObject({
      state: "failed",
      lastErrorCode: "provenance-invalid",
    });
    // A later grant refresh (settings page open, usage poll) must not wipe
    // the launch diagnostic — this exact wipe hid the v1.0.16 provenance
    // drift behind failed+null on real installs.
    const refreshed = await manager.eligibilityRead(true);
    expect(refreshed.state).toBe("allowed");
    expect(manager.status()).toMatchObject({
      state: "failed",
      lastErrorCode: "provenance-invalid",
    });
  });

  test("a package without the bridge artifact is blocked without spawning", async () => {
    const fixture = fakeManagerDeps();
    fixture.deps.resolveAndVerifyArtifact = async () => {
      throw new AccountBridgeError("artifact-missing");
    };
    const manager = new AccountBridgeManager(fixture.deps);
    await expect(manager.ensure()).rejects.toMatchObject({
      code: "artifact-missing",
    });
    expect(manager.status()).toMatchObject({
      state: "blocked",
      lastErrorCode: "artifact-missing",
    });
    expect(fixture.spawns).toHaveLength(0);
  });

  test("start-failure forensics keep only the latest five sidecars", async () => {
    const fixture = fakeManagerDeps();
    let attempt = 0;
    fixture.deps.spawnSidecar = async input => {
      fixture.spawns.push(input);
      attempt += 1;
      const exitCode = attempt;
      return {
        pid: 5000 + exitCode,
        endpoint: Promise.reject(
          new AccountBridgeError("runtime-exited-before-ready"),
        ),
        exited: Promise.resolve(exitCode),
        terminate() {},
        kill() {},
        forensics: () => ({
          ...emptySidecarForensics(),
          exitCode,
        }),
      };
    };
    const manager = new AccountBridgeManager(fixture.deps);
    for (let index = 0; index < 6; index += 1) {
      await expect(manager.ensure()).rejects.toMatchObject({
        code: "runtime-exited-before-ready",
      });
    }
    await manager.refreshLocalDiagnostics();
    const bundle = fixture.diagnosticBundles.at(-1)!;
    expect(bundle.sidecarProcess?.exitCode).toBe(6);
    expect(
      bundle.sidecarProcessHistory.map(record => record.forensics?.exitCode),
    ).toEqual([2, 3, 4, 5, 6]);
    expect(
      bundle.sidecarProcessHistory.every(
        record => record.readyDurationMs === null,
      ),
    ).toBeTrue();
  });

  test("uses private bootstrap, random per-run keys, clean env, and force-kill stop", async () => {
    const fixture = fakeManagerDeps();
    const manager = new AccountBridgeManager(fixture.deps);
    const status = await manager.ensure();
    expect(status.state).toBe("ready");
    expect(status.protocolVersion).toBe(1);
    expect(fixture.spawns).toHaveLength(1);
    expect(fixture.spawns[0]?.env.HTTP_PROXY).toBeUndefined();
    expect(fixture.configs[0]?.managementKey).toHaveLength(43);
    expect(fixture.configs[0]?.inferenceKey).toHaveLength(43);
    expect(fixture.configs[0]?.managementKey).not.toBe(
      fixture.configs[0]?.inferenceKey,
    );
    const bootstrap = JSON.parse(fixture.bootstrapCopies[0]!.toString("utf8"));
    expect(Buffer.from(bootstrap.masterKey, "base64url")).toHaveLength(32);
    expect(bootstrap.eligibilityGrant.payload).toBeString();
    const grantPayload = JSON.parse(
      Buffer.from(
        bootstrap.eligibilityGrant.payload,
        "base64url",
      ).toString("utf8"),
    );
    expect(bootstrap.requestNonce).toBe(grantPayload.client.requestNonce);
    expect(grantPayload.client).toEqual({
      requestNonce: bootstrap.requestNonce,
      ...RELEASE_IDENTITY,
    });
    expect(bootstrap.connectorPolicies).toHaveLength(4);
    // USER-OVERRIDE-2026-07-16: the bootstrap carries the locally derived
    // effective policy — control-plane process flags never disable shipped
    // connectors. Google's fixedArtifactVerified still mirrors the locally
    // verified plugin evidence (absent in this fixture's artifact mock).
    for (const policy of bootstrap.connectorPolicies as any[]) {
      expect(policy.featureEnabled).toBeTrue();
      expect(policy.termsStatus).toBe("signed-off");
      expect(policy.conformancePassed).toBeTrue();
      expect(policy.fixedArtifactVerified).toBe(policy.connectorId !== "google");
    }
    expect(bootstrap.connectorPolicies.map((policy: any) => ({
      connectorId: policy.connectorId,
      displayName: policy.displayName,
      authMode: policy.authMode,
    }))).toEqual([
      { connectorId: "openai", displayName: "OpenAI", authMode: "browser" },
      { connectorId: "anthropic", displayName: "Anthropic", authMode: "browser" },
      { connectorId: "google", displayName: "Google", authMode: "browser" },
      { connectorId: "xai", displayName: "xAI", authMode: "device-code" },
    ]);
    expect(JSON.stringify(status)).not.toContain(fixture.configs[0]!.managementKey);
    expect(JSON.stringify(status)).not.toContain(fixture.configs[0]!.inferenceKey);
    expect(fixture.removedRuntimeDirs).toEqual([]);

    const stopped = await manager.stop();
    expect(stopped.state).toBe("stopped");
    expect(fixture.stopCalls.terminate).toBe(1);
    expect(fixture.stopCalls.kill).toBe(1);
    expect(fixture.stopCalls.released).toBe(1);
    expect(fixture.removedRuntimeDirs).toEqual(["/private/runtime"]);
  });

  test("process shutdown removes network subscription and awaits the killed child exit witness", async () => {
    const fixture = fakeManagerDeps();
    const exactExit = deferred<number | null>();
    let unsubscribed = 0;
    fixture.deps.subscribeNetworkChanges = () => () => {
      unsubscribed += 1;
    };
    fixture.deps.spawnSidecar = async input => {
      fixture.spawns.push(input);
      return {
        pid: 4321,
        endpoint: Promise.resolve("http://127.0.0.1:43123"),
        exited: exactExit.promise,
        terminate() {
          fixture.stopCalls.terminate += 1;
        },
        kill() {
          fixture.stopCalls.kill += 1;
        },
        forensics: emptySidecarForensics,
      };
    };

    const manager = new AccountBridgeManager(fixture.deps);
    await manager.ensure();
    const shutdown = manager.shutdownForProcess();
    let settled = false;
    void shutdown.then(() => {
      settled = true;
    });
    for (
      let attempt = 0;
      attempt < 50 && fixture.stopCalls.kill === 0;
      attempt += 1
    ) {
      await Bun.sleep(1);
    }
    expect(unsubscribed).toBe(1);
    expect(fixture.stopCalls.terminate).toBe(1);
    expect(fixture.stopCalls.kill).toBe(1);
    expect(settled).toBeFalse();

    exactExit.resolve(null);
    await expect(shutdown).resolves.toMatchObject({ state: "stopped" });
    expect(fixture.stopCalls.released).toBe(1);
  });

  test("process shutdown does not create an absent singleton", async () => {
    _setAccountBridgeManagerForTest(null);
    await expect(shutdownAccountBridgeManagerForProcess()).resolves.toEqual({
      state: "not-created",
    });
  });

  test("keeps disabled connectors disabled and denies turn capability", async () => {
    const fixture = fakeManagerDeps({ enabled: false });
    const manager = new AccountBridgeManager(fixture.deps);
    await manager.ensure();
    expect(await manager.connectorList()).toEqual([connector(false)]);
    await expect(manager.turnAccess(ROUTE_A)).rejects.toMatchObject({
      code: "route-capability-denied",
    });
    await manager.stop();
  });

  test("narrows Google to disabled when the fixed plugin artifact is absent", async () => {
    const fixture = fakeManagerDeps({
      enabled: true,
      googleDirectoryEnabled: true,
    });
    const manager = new AccountBridgeManager(fixture.deps);
    await manager.ensure();
    const bootstrap = JSON.parse(fixture.bootstrapCopies[0]!.toString("utf8"));
    const openai = bootstrap.connectorPolicies.find(
      (policy: any) => policy.connectorId === "openai",
    );
    const google = bootstrap.connectorPolicies.find(
      (policy: any) => policy.connectorId === "google",
    );
    expect(openai.fixedArtifactVerified).toBeTrue();
    expect(google.fixedArtifactVerified).toBeFalse();
    await manager.stop();
  });

  test("private turn access is exact and requires chat+tools", async () => {
    const fixture = fakeManagerDeps({ enabled: true });
    const manager = new AccountBridgeManager(fixture.deps);
    await manager.ensure();
    const access = await manager.turnAccess(ROUTE_A);
    expect(access.endpoint).toBe("http://127.0.0.1:43123/v1/messages");
    expect(access.route.routeId).toBe(ROUTE_A);
    expect(access.route.chatRuntimeSupported).toBeTrue();
    expect(access.route.supportsTools).toBeTrue();
    expect(access.inferenceKey).toHaveLength(43);
    expect(
      fixture.httpURLs.some(
        url =>
          url.includes("/usage?") &&
          url.includes(`routeId=${ROUTE_A}`) &&
          url.includes("forceRefresh=true"),
      ),
    ).toBeTrue();
    await expect(manager.turnAccess(ROUTE_B)).rejects.toMatchObject({
      code: "unknown-account-route",
    });
    await manager.stop();
  });

  test("denies a missing, rebound, or non-ready account before minting turn access", async () => {
    const accountBase = {
      accountId: "opaque-account",
      connectorId: "openai",
      displayLabel: "Account AAAAAAAA",
      status: "ready",
      connectedAt: "2026-07-13T00:00:00.000Z",
      lastUsedAt: null,
      cooldownUntil: null,
    };
    const cases: Array<{ accountResponse: unknown; code: string }> = [
      { accountResponse: { accounts: [] }, code: "account-route-unavailable" },
      {
        accountResponse: {
          accounts: [{ ...accountBase, connectorId: "anthropic" }],
        },
        code: "account-route-binding-invalid",
      },
      ...([
        "reauthorization-required",
        "cooldown",
        "quota-exhausted",
        "disabled",
      ] as const).map(status => ({
        accountResponse: { accounts: [{ ...accountBase, status }] },
        code: "account-not-ready",
      })),
    ];

    for (const item of cases) {
      const fixture = fakeManagerDeps({
        enabled: true,
        accountResponse: item.accountResponse,
      });
      const manager = new AccountBridgeManager(fixture.deps);
      await manager.ensure();
      await expect(manager.turnAccess(ROUTE_A)).rejects.toMatchObject({
        code: item.code,
      });
      await manager.stop();
    }
  });

  test("denies fresh explicit cooldown/exhausted usage but permits missing or unknown usage", async () => {
    for (const [state, code] of [
      ["cooldown", "route-cooldown"],
      ["exhausted", "route-quota-exhausted"],
    ] as const) {
      const fixture = fakeManagerDeps({
        enabled: true,
        usageResponse: { snapshots: [usage(state)] },
      });
      const manager = new AccountBridgeManager(fixture.deps);
      await manager.ensure();
      await expect(manager.turnAccess(ROUTE_A)).rejects.toMatchObject({ code });
      await manager.stop();
    }

    for (const snapshots of [[], [usage("unknown")]]) {
      const fixture = fakeManagerDeps({
        enabled: true,
        usageResponse: { snapshots },
      });
      const manager = new AccountBridgeManager(fixture.deps);
      await manager.ensure();
      await expect(manager.turnAccess(ROUTE_A)).resolves.toMatchObject({
        route: { routeId: ROUTE_A },
      });
      await manager.stop();
    }
  });

  test("usage availability is optional, while malformed or rebound usage fails closed", async () => {
    const unavailable = fakeManagerDeps({ enabled: true });
    const unavailableHTTP = unavailable.deps.http;
    unavailable.deps.http = async input => {
      if (input.url.includes("/usage")) {
        throw new AccountBridgeError("facade-http-503");
      }
      return unavailableHTTP(input);
    };
    const availableManager = new AccountBridgeManager(unavailable.deps);
    await availableManager.ensure();
    await expect(availableManager.turnAccess(ROUTE_A)).resolves.toMatchObject({
      route: { routeId: ROUTE_A },
    });
    await availableManager.stop();

    for (const status of [425, 429]) {
      const refused = fakeManagerDeps({ enabled: true });
      const refusedHTTP = refused.deps.http;
      refused.deps.http = async input => {
        if (input.url.includes("/usage")) {
          throw new AccountBridgeError(`facade-http-${status}`);
        }
        return refusedHTTP(input);
      };
      const refusedManager = new AccountBridgeManager(refused.deps);
      await refusedManager.ensure();
      await expect(refusedManager.turnAccess(ROUTE_A)).rejects.toMatchObject({
        code: "usage-response-invalid",
      });
      await refusedManager.stop();
    }

    for (const usageResponse of [
      { snapshots: [{ ...usage(), accessToken: "forbidden" }] },
      { snapshots: [usage("available", ROUTE_B)] },
      { snapshots: [usage("available", ROUTE_A, "other-account")] },
    ]) {
      const fixture = fakeManagerDeps({ enabled: true, usageResponse });
      const manager = new AccountBridgeManager(fixture.deps);
      await manager.ensure();
      await expect(manager.turnAccess(ROUTE_A)).rejects.toMatchObject({
        code:
          "accessToken" in (usageResponse.snapshots[0] as Record<string, unknown>)
            ? "usage-response-invalid"
            : "usage-route-mismatch",
      });
      await manager.stop();
    }
  });

  test("route capability drift is rejected on every fresh preflight", async () => {
    for (const drift of [
      { chatRuntimeSupported: false },
      { chatRuntimeSupported: null },
      { supportsTools: false },
      { supportsTools: null },
    ]) {
      const fixture = fakeManagerDeps({
        enabled: true,
        routeResponse: { routes: [{ ...route(), ...drift }] },
      });
      const manager = new AccountBridgeManager(fixture.deps);
      await manager.ensure();
      await expect(manager.validateRouteAccess(ROUTE_A)).rejects.toMatchObject({
        code: "route-capability-denied",
      });
      await manager.stop();
    }
  });

  test("serializes a live policy refresh until delayed child exit with no overlapping spawn", async () => {
    const fixture = fakeManagerDeps({ enabled: true });
    const stopTimeout = deferred<void>();
    fixture.deps.sleep = async () => stopTimeout.promise;
    let fetches = 0;
    const fetchControlPlane = fixture.deps.fetchControlPlane;
    fixture.deps.fetchControlPlane = async request => {
      fetches += 1;
      return fetchControlPlane(request);
    };
    const manager = new AccountBridgeManager(fixture.deps);
    await manager.ensure();
    expect(fetches).toBe(1);

    const refresh = manager.eligibilityRead(true);
    for (
      let attempt = 0;
      attempt < 50 && fixture.stopCalls.terminate === 0;
      attempt++
    ) {
      await Bun.sleep(1);
    }
    expect(fixture.stopCalls.terminate).toBe(1);

    let ensureFinished = false;
    let turnFinished = false;
    const ensureDuringRefresh = manager.ensure().then(status => {
      ensureFinished = true;
      return status;
    });
    const turnDuringRefresh = manager.turnAccess(ROUTE_A).then(access => {
      turnFinished = true;
      return access;
    });
    await Bun.sleep(5);
    expect(fixture.spawns).toHaveLength(1);
    expect(fetches).toBe(1);
    expect(ensureFinished).toBeFalse();
    expect(turnFinished).toBeFalse();

    fixture.exits[0]!.resolve(0);
    const [, ensured, access] = await Promise.all([
      refresh,
      ensureDuringRefresh,
      turnDuringRefresh,
    ]);
    expect(fixture.spawns).toHaveLength(2);
    expect(fetches).toBe(2);
    expect(ensured.state).toBe("ready");
    expect(access.endpoint).toBe("http://127.0.0.1:43124/v1/messages");

    fixture.deps.sleep = async () => {};
    await manager.stop();
  });

  test("fails closed without loading policy or spawning when the old child cannot exit", async () => {
    const fixture = fakeManagerDeps({ enabled: true });
    let fetches = 0;
    const fetchControlPlane = fixture.deps.fetchControlPlane;
    fixture.deps.fetchControlPlane = async request => {
      fetches += 1;
      return fetchControlPlane(request);
    };
    const manager = new AccountBridgeManager(fixture.deps);
    await manager.ensure();
    fixture.children[0]!.kill = () => {
      fixture.stopCalls.kill += 1;
    };

    await expect(manager.eligibilityRead(true)).rejects.toMatchObject({
      code: "runtime-stop-incomplete",
    });
    expect(fetches).toBe(1);
    expect(fixture.spawns).toHaveLength(1);
    expect(manager.status()).toMatchObject({
      state: "failed",
      lastErrorCode: "runtime-stop-incomplete",
    });

    fixture.exits[0]!.resolve(0);
    await Bun.sleep(1);
    await manager.stop();
  });

  test("control-plane process flags cannot disable shipped connectors (USER-OVERRIDE-2026-07-16)", async () => {
    let enabledConnectors: readonly string[] = ["openai", "anthropic"];
    const fixture = fakeManagerDeps({
      enabledConnectors: () => enabledConnectors,
    });
    const manager = new AccountBridgeManager(fixture.deps);
    await manager.ensure();

    enabledConnectors = ["anthropic"];
    const refreshed = await manager.eligibilityRead(true);
    expect(refreshed.state).toBe("allowed");
    expect(manager.status().state).toBe("ready");
    expect(fixture.spawns).toHaveLength(2);
    // The refreshed directory flips openai's process flags to false; the
    // relaunched bootstrap must still carry the fully enabled effective
    // policy — connector enablement is local truth, not a remote flag.
    const nextBootstrap = JSON.parse(
      fixture.bootstrapCopies[1]!.toString("utf8"),
    );
    for (const policy of nextBootstrap.connectorPolicies as any[]) {
      expect(policy.featureEnabled).toBeTrue();
      expect(policy.termsStatus).toBe("signed-off");
      expect(policy.conformancePassed).toBeTrue();
    }
    await manager.stop();
  });

  test("fails closed if the request nonce source replays an issued nonce", async () => {
    const fixture = fakeManagerDeps({ enabled: true });
    let randomCall = 0;
    fixture.deps.randomBytes = size => {
      randomCall += 1;
      const marker = randomCall === 4 ? 8 : randomCall + 7;
      return new Uint8Array(size).fill(marker);
    };
    const manager = new AccountBridgeManager(fixture.deps);
    await manager.ensure();

    const eligibility = await manager.eligibilityRead(true);
    expect(eligibility.state).toBe("unavailable");
    expect(eligibility.reasonCode).toBe("control-plane-request-nonce-reused");
    expect(manager.status().state).toBe("blocked");
    expect(fixture.spawns).toHaveLength(1);
    await manager.stop();
  });

  test("revalidates the signed connector directory before every turn", async () => {
    let now = NOW;
    const fixture = fakeManagerDeps({
      enabled: true,
      now: () => now,
      connectorExpiresAt: Math.floor(NOW / 1_000) + 1,
    });
    let fetches = 0;
    const fetchControlPlane = fixture.deps.fetchControlPlane;
    fixture.deps.fetchControlPlane = async request => {
      fetches += 1;
      return fetchControlPlane(request);
    };
    const manager = new AccountBridgeManager(fixture.deps);
    await manager.ensure();
    expect(fetches).toBe(1);
    now += 2_000;
    await expect(manager.turnAccess(ROUTE_A)).rejects.toMatchObject({
      code: "eligibility-denied",
    });
    expect(fetches).toBe(2);
    await manager.stop();
  });

  test("public DTO parser rejects a secret-bearing facade response", async () => {
    const fixture = fakeManagerDeps({
      enabled: true,
      routeResponse: { routes: [{ ...route(), accessToken: "must-not-cross" }] },
    });
    const manager = new AccountBridgeManager(fixture.deps);
    await manager.ensure();
    await expect(manager.modelList()).rejects.toThrow("forbidden field accessToken");
    await manager.stop();
  });

  test("writes a status-triggered local diagnostic bundle with only hashed identity refs", async () => {
    const rawIdentity = "private.person@example.test";
    const rawModel = "raw-subject-model";
    const fixture = fakeManagerDeps({
      enabled: true,
      connectorResponse: {
        connectors: [
          {
            ...connector(true),
            displayName: rawIdentity,
          },
        ],
      },
      accountResponse: {
        accounts: [
          {
            accountId: rawIdentity,
            connectorId: "openai",
            displayLabel: rawIdentity,
            status: "cooldown",
            connectedAt: "2026-07-13T00:00:00.000Z",
            lastUsedAt: null,
            cooldownUntil: "2026-07-13T12:30:00.000Z",
          },
        ],
      },
      routeResponse: {
        routes: [
          {
            ...route(),
            accountId: rawIdentity,
            modelId: rawModel,
            displayName: rawIdentity,
            accountLabel: rawIdentity,
          },
        ],
      },
      usageResponse: {
        snapshots: [
          {
            routeId: ROUTE_A,
            accountId: rawIdentity,
            state: "cooldown",
            remainingPercent: null,
            limitingWindowLabel: null,
            resetsAt: "2026-07-13T12:30:00.000Z",
            windows: [
              {
                label: "model-runtime-cooldown",
                limit: null,
                used: null,
                remainingPercent: null,
                resetsAt: "2026-07-13T12:30:00.000Z",
              },
            ],
            observedAt: "2026-07-13T12:00:00.000Z",
          },
        ],
      },
    });
    const manager = new AccountBridgeManager(fixture.deps);
    await manager.ensure();
    await manager.refreshLocalDiagnostics();

    expect(fixture.diagnosticBundles).toHaveLength(1);
    const bundle = fixture.diagnosticBundles[0]!;
    const serialized = JSON.stringify(bundle);
    expect(bundle.runtime.state).toBe("ready");
    expect(bundle.sidecar?.accounts[0]?.accountRef).toMatch(/^account-[a-f0-9]{16}$/);
    expect(bundle.sidecar?.routes[0]?.routeRef).toMatch(/^route-[a-f0-9]{16}$/);
    expect(bundle.sidecar?.routes[0]?.modelRef).toMatch(/^model-[a-f0-9]{16}$/);
    expect(bundle.sidecar?.routes[0]?.accountRef).toBe(
      bundle.sidecar?.accounts[0]?.accountRef,
    );
    expect(bundle.sidecar?.usage[0]?.accountRef).toBe(
      bundle.sidecar?.accounts[0]?.accountRef,
    );
    expect(bundle.sidecar?.usage[0]?.routeRef).toBe(
      bundle.sidecar?.routes[0]?.routeRef,
    );
    expect(bundle.sidecar?.usage[0]?.state).toBe("cooldown");
    expect(bundle.collectionErrorCode).toBeNull();
    expect(
      fixture.httpURLs.some(url => url.includes("/usage?forceRefresh=true")),
    ).toBe(true);
    for (const forbidden of [
      rawIdentity,
      rawModel,
      ROUTE_A,
      "displayLabel",
      "accountLabel",
      "authorizationUrl",
      "managementKey",
      "inferenceKey",
      "signedGrant",
      "access_token",
    ]) {
      expect(serialized).not.toContain(forbidden);
    }
    expect(serialized).not.toMatch(
      /"(?:token|tokens|accessToken|access_token|refreshToken|refresh_token|oauthToken|oauth_token|email|subject|rawSubject|raw_subject|key|keys|apiKey|api_key|managementKey|management_key|inferenceKey|inference_key|grant|grants|signedGrant|signed_grant|eligibilityGrant|eligibility_grant)"\s*:/i,
    );
    await manager.stop();
  });

  test("diagnostic collection drops secret-bearing errors and writes only a stable code", async () => {
    const fixture = fakeManagerDeps({ enabled: true });
    const manager = new AccountBridgeManager(fixture.deps);
    await manager.ensure();
    const http = fixture.deps.http;
    fixture.deps.http = async input => {
      if (input.url.includes("/usage")) {
        throw new Error(
          "access_token=must-not-persist private.person@example.test raw-subject",
        );
      }
      return http(input);
    };
    await manager.refreshLocalDiagnostics();
    const bundle = fixture.diagnosticBundles[0]!;
    const serialized = JSON.stringify(bundle);
    expect(bundle.sidecar).toBeNull();
    expect(bundle.collectionErrorCode).toBe("sidecar-snapshot-unavailable");
    expect(serialized).not.toContain("must-not-persist");
    expect(serialized).not.toContain("private.person@example.test");
    expect(serialized).not.toContain("raw-subject");
    await manager.stop();
  });

  test("diagnostic writer uses the existing diagnostics directory and applies platform file controls", async () => {
    const fixture = fakeManagerDeps({ enabled: true });
    const manager = new AccountBridgeManager(fixture.deps);
    await manager.ensure();
    await manager.refreshLocalDiagnostics();
    const bundle = fixture.diagnosticBundles[0]!;
    const root = await fs.mkdtemp(join(tmpdir(), "account-bridge-diag-"));
    try {
      await writeAccountBridgeDiagnosticBundle(bundle, root);
      const target = join(root, "diagnostics", "account-bridge.json");
      const stat = await fs.lstat(target);
      expect(stat.isFile()).toBe(true);
      expect(JSON.parse(await fs.readFile(target, "utf8"))).toEqual(bundle);

      if (process.platform !== "win32") {
        expect(stat.mode & 0o777).toBe(0o600);
        const outside = join(root, "outside.json");
        await fs.writeFile(outside, "unchanged", { mode: 0o600 });
        await fs.rm(target);
        await fs.symlink(outside, target);
        await expect(
          writeAccountBridgeDiagnosticBundle(bundle, root),
        ).rejects.toMatchObject({ code: "diagnostic-target-invalid" });
        expect(await fs.readFile(outside, "utf8")).toBe("unchanged");
      }
    } finally {
      await manager.stop();
      await fs.rm(root, { recursive: true, force: true });
    }
  });

  test("restart schedule is exactly 1/2/4 seconds and then explicit retry", () => {
    expect(accountBridgeAutomaticRestartDelay([NOW], NOW)).toBe(1_000);
    expect(accountBridgeAutomaticRestartDelay([NOW, NOW], NOW)).toBe(2_000);
    expect(accountBridgeAutomaticRestartDelay([NOW, NOW, NOW], NOW)).toBe(4_000);
    expect(accountBridgeAutomaticRestartDelay([NOW, NOW, NOW, NOW], NOW)).toBeNull();
    expect(
      accountBridgeAutomaticRestartDelay([NOW - 10 * 60_000 - 1, NOW], NOW),
    ).toBe(1_000);
  });

  test("normalizes upstream login start and validates the exact completed opaque account", async () => {
    const fixture = fakeManagerDeps({ enabled: true });
    const originalHTTP = fixture.deps.http;
    const existingAccountId = "E".repeat(43);
    const completedAccountId = "N".repeat(43);
    let completed = false;
    let accountReads = 0;
    fixture.deps.http = async input => {
      if (input.url.includes("/login/start")) {
        return {
          status: "ok",
          url: "https://auth.example.test/authorize",
          state: "oauth-session-123",
        };
      }
      if (input.url.includes("/login/poll")) {
        completed = true;
        return {
          state: "succeeded",
          accountId: completedAccountId,
          errorCode: null,
        };
      }
      if (input.url.includes("/accounts")) {
        accountReads += 1;
        if (!completed) return originalHTTP(input);
        return {
          accounts: [
            {
              accountId: existingAccountId,
              connectorId: "openai",
              displayLabel: "Account AAAAAAAA",
              status: "ready",
              connectedAt: "2026-07-13T00:00:00.000Z",
              lastUsedAt: null,
              cooldownUntil: null,
            },
            {
              accountId: completedAccountId,
              connectorId: "openai",
              displayLabel: "Account BBBBBBBB",
              status: "ready",
              connectedAt: "2026-07-13T00:01:00.000Z",
              lastUsedAt: null,
              cooldownUntil: null,
            },
          ],
        };
      }
      return originalHTTP(input);
    };
    const manager = new AccountBridgeManager(fixture.deps);
    await manager.ensure();
    expect(await manager.loginStart("openai")).toEqual({
      sessionId: "oauth-session-123",
      authMode: "browser",
      authorizationUrl: "https://auth.example.test/authorize",
      userCode: null,
      verificationUrl: null,
      expiresAt: null,
    });
    expect(await manager.loginPoll("oauth-session-123")).toEqual({
      state: "succeeded",
      accountId: completedAccountId,
      errorCode: null,
    });
    expect(accountReads).toBe(1);
    await manager.stop();
  });

  test("reauthorization keeps the existing exact account despite a concurrent unrelated addition", async () => {
    const fixture = fakeManagerDeps({ enabled: true });
    const originalHTTP = fixture.deps.http;
    const reauthorizedAccountId = "R".repeat(43);
    const concurrentAccountId = "C".repeat(43);
    fixture.deps.http = async input => {
      if (input.url.includes("/login/start")) {
        return {
          status: "ok",
          url: "https://auth.example.test/authorize",
          state: "oauth-reauthorization-session",
        };
      }
      if (input.url.includes("/login/poll")) {
        return {
          state: "succeeded",
          accountId: reauthorizedAccountId,
          errorCode: null,
        };
      }
      if (input.url.includes("/accounts")) {
        return {
          accounts: [
            {
              accountId: reauthorizedAccountId,
              connectorId: "openai",
              displayLabel: "Account RRRRRRRR",
              status: "ready",
              connectedAt: "2026-07-13T00:00:00.000Z",
              lastUsedAt: null,
              cooldownUntil: null,
            },
            {
              accountId: concurrentAccountId,
              connectorId: "openai",
              displayLabel: "Account CCCCCCCC",
              status: "ready",
              connectedAt: "2026-07-13T00:01:00.000Z",
              lastUsedAt: null,
              cooldownUntil: null,
            },
          ],
        };
      }
      return originalHTTP(input);
    };

    const manager = new AccountBridgeManager(fixture.deps);
    await manager.ensure();
    await manager.loginStart("openai");
    await expect(
      manager.loginPoll("oauth-reauthorization-session"),
    ).resolves.toEqual({
      state: "succeeded",
      accountId: reauthorizedAccountId,
      errorCode: null,
    });
    await manager.stop();
  });

  test("rejects a succeeded login without a canonical exact account association", async () => {
    const fixture = fakeManagerDeps({ enabled: true });
    const originalHTTP = fixture.deps.http;
    fixture.deps.http = async input => {
      if (input.url.includes("/login/start")) {
        return {
          status: "ok",
          url: "https://auth.example.test/authorize",
          state: "oauth-missing-association-session",
        };
      }
      if (input.url.includes("/login/poll")) {
        return { state: "succeeded", accountId: null, errorCode: null };
      }
      return originalHTTP(input);
    };

    const manager = new AccountBridgeManager(fixture.deps);
    await manager.ensure();
    await manager.loginStart("openai");
    await expect(
      manager.loginPoll("oauth-missing-association-session"),
    ).rejects.toMatchObject({
      code: "login-account-association-unavailable",
    });
    await manager.stop();
  });

  test("rejects an exact account association rebound to another connector", async () => {
    const fixture = fakeManagerDeps({ enabled: true });
    const originalHTTP = fixture.deps.http;
    const reboundAccountId = "B".repeat(43);
    fixture.deps.http = async input => {
      if (input.url.includes("/login/start")) {
        return {
          status: "ok",
          url: "https://auth.example.test/authorize",
          state: "oauth-rebound-association-session",
        };
      }
      if (input.url.includes("/login/poll")) {
        return {
          state: "succeeded",
          accountId: reboundAccountId,
          errorCode: null,
        };
      }
      if (input.url.includes("/accounts")) {
        return {
          accounts: [{
            accountId: reboundAccountId,
            connectorId: "anthropic",
            displayLabel: "Account BBBBBBBB",
            status: "ready",
            connectedAt: "2026-07-13T00:00:00.000Z",
            lastUsedAt: null,
            cooldownUntil: null,
          }],
        };
      }
      return originalHTTP(input);
    };

    const manager = new AccountBridgeManager(fixture.deps);
    await manager.ensure();
    await manager.loginStart("openai");
    await expect(
      manager.loginPoll("oauth-rebound-association-session"),
    ).rejects.toMatchObject({
      code: "login-account-association-unavailable",
    });
    await manager.stop();
  });

  test("normalizes xAI device-flow fields without retaining raw response", async () => {
    const fixture = fakeManagerDeps();
    const originalHTTP = fixture.deps.http;
    fixture.deps.http = async input => {
      if (input.url.includes("/connectors")) {
        return {
          connectors: [
            {
              connectorId: "xai",
              displayName: "xAI",
              authMode: "device-code",
              enabled: true,
              disabledReasonCode: null,
              termsStatus: "signed-off",
            },
          ],
        };
      }
      if (input.url.includes("/accounts")) return { accounts: [] };
      if (input.url.includes("/login/start")) {
        return {
          status: "ok",
          url: "https://auth.x.ai/device",
          state: "xai-session-123",
          flow: "device",
          user_code: "SAFE-CODE",
          expires_in: 600,
        };
      }
      return originalHTTP(input);
    };
    const manager = new AccountBridgeManager(fixture.deps);
    await manager.ensure();
    const view = await manager.loginStart("xai");
    expect(view).toEqual({
      sessionId: "xai-session-123",
      authMode: "device-code",
      authorizationUrl: null,
      userCode: "SAFE-CODE",
      verificationUrl: "https://auth.x.ai/device",
      expiresAt: new Date(NOW + 600_000).toISOString(),
    });
    await manager.stop();
  });

  test("invalidates, revalidates, and restarts the sidecar on network change", async () => {
    const fixture = fakeManagerDeps();
    let notifyNetworkChange: (() => void) | null = null;
    let fetches = 0;
    const originalFetch = fixture.deps.fetchControlPlane;
    fixture.deps.fetchControlPlane = async request => {
      fetches += 1;
      return originalFetch(request);
    };
    fixture.deps.subscribeNetworkChanges = listener => {
      notifyNetworkChange = listener;
      return () => {};
    };
    const manager = new AccountBridgeManager(fixture.deps);
    await manager.ensure();
    expect(fetches).toBe(1);
    notifyNetworkChange!();
    for (
      let attempt = 0;
      attempt < 50 &&
      (fetches < 2 || fixture.spawns.length < 2 ||
        manager.status().state !== "ready");
      attempt++
    ) {
      await Bun.sleep(1);
    }
    expect(fetches).toBe(2);
    expect(fixture.spawns).toHaveLength(2);
    expect(manager.status().state).toBe("ready");
    expect(fixture.stopCalls.terminate).toBe(1);
    await manager.stop();
  });
});

describe("Account Bridge detection consent gate", () => {
  function countedFetches(fixture: ReturnType<typeof fakeManagerDeps>): () => number {
    let fetches = 0;
    const fetchControlPlane = fixture.deps.fetchControlPlane;
    fixture.deps.fetchControlPlane = async request => {
      fetches += 1;
      return fetchControlPlane(request);
    };
    return () => fetches;
  }

  test("without consent every read is consent-required and no control-plane request leaves", async () => {
    const fixture = fakeManagerDeps({ readEligibilityConsent: () => null });
    const fetches = countedFetches(fixture);
    const manager = new AccountBridgeManager(fixture.deps);

    expect(await manager.eligibilityRead(false)).toEqual({
      state: "unavailable",
      countryCode: null,
      policyVersion: null,
      checkedAt: null,
      expiresAt: null,
      reasonCode: "consent-required",
    });
    expect((await manager.eligibilityRead(true)).reasonCode).toBe(
      "consent-required",
    );
    expect(fetches()).toBe(0);
    expect(fixture.spawns).toHaveLength(0);
  });

  test("ensure() without consent fails into the eligibility-denied blocked path", async () => {
    const fixture = fakeManagerDeps({ readEligibilityConsent: () => null });
    const fetches = countedFetches(fixture);
    const manager = new AccountBridgeManager(fixture.deps);

    await expect(manager.ensure()).rejects.toMatchObject({
      code: "eligibility-denied",
    });
    expect(manager.status()).toMatchObject({
      state: "blocked",
      lastErrorCode: "consent-required",
    });
    expect(fetches()).toBe(0);
    expect(fixture.spawns).toHaveLength(0);
  });

  test("a granted consent leaves the normal flow untouched", async () => {
    const fixture = fakeManagerDeps();
    const manager = new AccountBridgeManager(fixture.deps);
    expect((await manager.eligibilityRead(false)).state).toBe("allowed");
    const ensured = await manager.ensure();
    expect(ensured.state).toBe("ready");
    await manager.stop();
  });

  test("withdrawal between reads gates the second read despite a valid cached grant", async () => {
    let consent: AccountBridgeEligibilityConsent | null = {
      grantedAtIso: "2026-07-13T00:00:00.000Z",
      noticeVersion: 1,
    };
    const fixture = fakeManagerDeps({
      readEligibilityConsent: () => consent,
    });
    const fetches = countedFetches(fixture);
    const manager = new AccountBridgeManager(fixture.deps);

    expect((await manager.eligibilityRead(false)).state).toBe("allowed");
    expect(fetches()).toBe(1);

    consent = null;
    const gated = await manager.eligibilityRead(false);
    expect(gated.state).toBe("unavailable");
    expect(gated.reasonCode).toBe("consent-required");
    // Neither a fresh fetch nor the still-valid cached grant may surface.
    expect(gated.countryCode).toBeNull();
    expect(fetches()).toBe(1);
  });

  test("a consent granted against an older notice version is not consent", async () => {
    const fixture = fakeManagerDeps({
      readEligibilityConsent: () => ({
        grantedAtIso: "2026-07-13T00:00:00.000Z",
        noticeVersion: 0,
      }),
    });
    const fetches = countedFetches(fixture);
    const manager = new AccountBridgeManager(fixture.deps);
    expect((await manager.eligibilityRead(false)).reasonCode).toBe(
      "consent-required",
    );
    expect(fetches()).toBe(0);
  });
});

describe("Account Bridge v2 per-connector region gating", () => {
  test("accepts a signed v2 directory and fills v1 regionPolicy from the client map", () => {
    const v2 = signedConnectorDirectory(false, false, {}, payload => {
      payload.version = "v2";
      payload.connectors = v2DirectoryConnectors();
    });
    const verified = verifyConnectorPolicyDirectoryWithKey(
      v2.directory,
      v2.publicKey,
      TEST_CONTROL_PLANE_REQUEST,
      NOW,
    );
    expect(verified.policies).toHaveLength(7);
    expect(verified.policies.map(policy => policy.regionPolicy)).toEqual([
      "non-cn",
      "non-cn",
      "non-cn",
      "non-cn",
      "global",
      "global",
      "global",
    ]);

    const v1 = signedConnectorDirectory();
    const verifiedV1 = verifyConnectorPolicyDirectoryWithKey(
      v1.directory,
      v1.publicKey,
      TEST_CONTROL_PLANE_REQUEST,
      NOW,
    );
    expect(verifiedV1.policies).toHaveLength(4);
    expect(
      verifiedV1.policies.every(policy => policy.regionPolicy === "non-cn"),
    ).toBeTrue();
  });

  test("rejects incomplete, oversized, regionPolicy-less, and cross-generation directories", () => {
    const sixEntries = signedConnectorDirectory(false, false, {}, payload => {
      payload.version = "v2";
      payload.connectors = v2DirectoryConnectors().slice(0, 6);
    });
    expect(() =>
      verifyConnectorPolicyDirectoryWithKey(
        sixEntries.directory,
        sixEntries.publicKey,
        TEST_CONTROL_PLANE_REQUEST,
        NOW,
      ),
    ).toThrow("connector-policy-directory-incomplete");

    const eightEntries = signedConnectorDirectory(false, false, {}, payload => {
      payload.version = "v2";
      const connectors = v2DirectoryConnectors();
      connectors.push({ ...connectors[0]! });
      payload.connectors = connectors;
    });
    expect(() =>
      verifyConnectorPolicyDirectoryWithKey(
        eightEntries.directory,
        eightEntries.publicKey,
        TEST_CONTROL_PLANE_REQUEST,
        NOW,
      ),
    ).toThrow("connector-policy-directory-incomplete");

    const missingRegionPolicy = signedConnectorDirectory(
      false,
      false,
      {},
      payload => {
        payload.version = "v2";
        const connectors = v2DirectoryConnectors();
        delete connectors[0]!.regionPolicy;
        payload.connectors = connectors;
      },
    );
    expect(() =>
      verifyConnectorPolicyDirectoryWithKey(
        missingRegionPolicy.directory,
        missingRegionPolicy.publicKey,
        TEST_CONTROL_PLANE_REQUEST,
        NOW,
      ),
    ).toThrow("connector-policy-directory-malformed");

    // v1 entries must stay exactly the legacy shape and legacy id set: a
    // v2-only id or a smuggled regionPolicy key is malformed.
    const v1WithNewId = signedConnectorDirectory(false, false, {}, payload => {
      payload.connectors[0]!.connectorId = "qwen";
    });
    expect(() =>
      verifyConnectorPolicyDirectoryWithKey(
        v1WithNewId.directory,
        v1WithNewId.publicKey,
        TEST_CONTROL_PLANE_REQUEST,
        NOW,
      ),
    ).toThrow("connector-policy-directory-malformed");
    const v1WithRegionPolicy = signedConnectorDirectory(
      false,
      false,
      {},
      payload => {
        payload.connectors[0]!.regionPolicy = "global";
      },
    );
    expect(() =>
      verifyConnectorPolicyDirectoryWithKey(
        v1WithRegionPolicy.directory,
        v1WithRegionPolicy.publicKey,
        TEST_CONTROL_PLANE_REQUEST,
        NOW,
      ),
    ).toThrow("connector-policy-directory-malformed");
  });

  test("a v2 grant with a v1 directory is a generation mismatch and stays unavailable", async () => {
    const fixture = fakeManagerDeps({
      generation: "v2",
      directoryGeneration: "v1",
    });
    const manager = new AccountBridgeManager(fixture.deps);
    const view = await manager.eligibilityRead(false);
    expect(view.state).toBe("unavailable");
    expect(view.reasonCode).toBe("control-plane-generation-mismatch");
    expect(manager.status()).toMatchObject({
      state: "blocked",
      lastErrorCode: "control-plane-generation-mismatch",
    });
    expect(fixture.spawns).toHaveLength(0);
  });

  test("v2 non-CN grant enables all seven and bootstraps seven region-tagged policies", async () => {
    const fixture = fakeManagerDeps({ generation: "v2" });
    const manager = new AccountBridgeManager(fixture.deps);

    expect((await manager.eligibilityRead(false)).state).toBe("allowed");
    const rows = await manager.connectorList();
    expect(rows.map(row => row.connectorId)).toEqual([...ALL_CONNECTOR_IDS]);
    for (const row of rows) {
      expect(row.enabled).toBeTrue();
      expect(row.disabledReasonCode).toBeNull();
    }

    await manager.ensure();
    const bootstrap = JSON.parse(fixture.bootstrapCopies[0]!.toString("utf8"));
    expect(bootstrap.connectorPolicies).toHaveLength(7);
    expect(
      bootstrap.connectorPolicies.map((policy: any) => [
        policy.connectorId,
        policy.regionPolicy,
      ]),
    ).toEqual([
      ["openai", "non-cn"],
      ["anthropic", "non-cn"],
      ["google", "non-cn"],
      ["xai", "non-cn"],
      ["qwen", "global"],
      ["kimi", "global"],
      ["zai", "global"],
    ]);
    await manager.stop();
  });

  test("v2 CN grant keeps blocked-cn aggregate, starts the runtime, and serves only global connectors", async () => {
    const fixture = fakeManagerDeps({
      generation: "v2",
      countryCode: "CN",
      regionAllowed: false,
      grantConnectors: ["qwen", "kimi", "zai"],
      connectorResponse: {
        connectors: [
          ...["openai", "anthropic", "google", "xai"].map(connectorId => ({
            connectorId,
            displayName: V2_CONNECTOR_DISPLAY_NAMES[connectorId]!,
            authMode: "browser",
            enabled: false,
            disabledReasonCode: "connector_region_blocked",
            termsStatus: "signed-off",
          })),
          ...["qwen", "kimi", "zai"].map(connectorId => ({
            connectorId,
            displayName: V2_CONNECTOR_DISPLAY_NAMES[connectorId]!,
            authMode: "device-code",
            enabled: true,
            disabledReasonCode: null,
            termsStatus: "signed-off",
          })),
        ],
      },
    });
    const baseHTTP = fixture.deps.http;
    fixture.deps.http = async input => {
      if (input.url.includes("/login/start")) {
        return {
          status: "ok",
          url: "https://auth.qwen.example/device",
          state: "qwen-session-1",
          flow: "device",
          user_code: "QWEN-CODE",
          expires_in: 600,
        };
      }
      return baseHTTP(input);
    };
    const manager = new AccountBridgeManager(fixture.deps);

    expect(await manager.eligibilityRead(false)).toMatchObject({
      state: "blocked-cn",
      countryCode: "CN",
    });
    // Signed-directory projection before start: legacy four disabled with
    // the region reason, the three global connectors enabled.
    const rows = await manager.connectorList();
    expect(rows.map(row => [row.connectorId, row.enabled])).toEqual([
      ["openai", false],
      ["anthropic", false],
      ["google", false],
      ["xai", false],
      ["qwen", true],
      ["kimi", true],
      ["zai", true],
    ]);
    for (const row of rows.slice(0, 4)) {
      expect(row.disabledReasonCode).toBe("connector_region_blocked");
    }
    for (const row of rows.slice(4)) {
      expect(row.disabledReasonCode).toBeNull();
    }

    // The runtime must start under CN+v2 (the sidecar re-enforces per
    // connector); loginStart then follows the ready sidecar's own rows.
    const status = await manager.ensure();
    expect(status.state).toBe("ready");
    await expect(manager.loginStart("openai")).rejects.toMatchObject({
      code: "connector-disabled",
    });
    const login = await manager.loginStart("qwen");
    expect(login.authMode).toBe("device-code");
    expect(login.userCode).toBe("QWEN-CODE");
    expect(login.verificationUrl).toBe("https://auth.qwen.example/device");
    await manager.stop();
  });

  test("client CN floor blocks a mis-issued non-cn connector in a v2 grant", async () => {
    const fixture = fakeManagerDeps({
      generation: "v2",
      countryCode: "CN",
      regionAllowed: false,
      grantConnectors: ["openai", "qwen"],
    });
    const manager = new AccountBridgeManager(fixture.deps);
    await manager.eligibilityRead(false);
    const rows = await manager.connectorList();
    const openai = rows.find(row => row.connectorId === "openai")!;
    expect(openai.enabled).toBeFalse();
    expect(openai.disabledReasonCode).toBe("connector_region_blocked");
    const qwen = rows.find(row => row.connectorId === "qwen")!;
    expect(qwen.enabled).toBeTrue();
    // The floor also blocks the action path before any sidecar exists.
    await expect(manager.loginStart("openai")).rejects.toMatchObject({
      code: "connector-disabled",
    });
    expect(fixture.spawns).toHaveLength(0);
  });
});

describe("Account Bridge fail-closed master key storage", () => {
  function memoryDeps(
    platform: "darwin" | "win32" | "linux",
    options: { initial?: string; readError?: boolean } = {},
  ): AccountBridgeMasterKeyDeps & {
    counters: { reads: number; writes: number; random: number; locks: number };
  } {
    let value = options.initial;
    let lockTail: Promise<void> = Promise.resolve();
    const counters = { reads: 0, writes: 0, random: 0, locks: 0 };
    const backend: AccountBridgeMasterKeyBackend = {
      async read() {
        counters.reads += 1;
        if (options.readError) return { status: "error" };
        return value === undefined
          ? { status: "absent" }
          : { status: "data", value };
      },
      async write(next) {
        counters.writes += 1;
        if (value !== undefined) throw new Error("unexpected overwrite");
        await Bun.sleep(1);
        value = next;
      },
    };
    return {
      platform,
      counters,
      randomBytes(size) {
        counters.random += 1;
        return new Uint8Array(size).fill(0x31);
      },
      async withLock<T>(operation: () => Promise<T>): Promise<T> {
        counters.locks += 1;
        const previous = lockTail;
        let release!: () => void;
        lockTail = new Promise<void>(resolve => {
          release = resolve;
        });
        await previous;
        try {
          return await operation();
        } finally {
          release();
        }
      },
      async createBackend() {
        return backend;
      },
    };
  }

  for (const platform of ["darwin", "win32", "linux"] as const) {
    test(`${platform} creates once and reuses one canonical secure-store key`, async () => {
      const deps = memoryDeps(platform);
      const first = await loadOrCreateAccountBridgeMasterKey(deps);
      const second = await loadOrCreateAccountBridgeMasterKey(deps);
      expect(Buffer.from(first).toString("hex")).toBe("31".repeat(32));
      expect(Buffer.from(second).toString("hex")).toBe("31".repeat(32));
      expect(deps.counters).toEqual({
        reads: 3,
        writes: 1,
        random: 1,
        locks: 2,
      });
    });
  }

  test("serializes concurrent first use and never creates competing keys", async () => {
    const deps = memoryDeps("win32");
    const keys = await Promise.all(
      Array.from({ length: 12 }, () => loadOrCreateAccountBridgeMasterKey(deps)),
    );
    expect(new Set(keys.map(key => Buffer.from(key).toString("hex"))).size).toBe(1);
    expect(deps.counters.writes).toBe(1);
    expect(deps.counters.random).toBe(1);
    expect(deps.counters.locks).toBe(12);
  });

  test("corrupt data and unavailable secure storage fail closed without overwrite", async () => {
    const corrupt = memoryDeps("linux", { initial: "not-a-master-key" });
    await expect(
      loadOrCreateAccountBridgeMasterKey(corrupt),
    ).rejects.toMatchObject({ code: "master-key-invalid" });
    expect(corrupt.counters.writes).toBe(0);
    expect(corrupt.counters.random).toBe(0);

    const unavailable = memoryDeps("darwin", { readError: true });
    await expect(
      loadOrCreateAccountBridgeMasterKey(unavailable),
    ).rejects.toMatchObject({
      code: "master-key-secure-storage-unavailable",
    });
    expect(unavailable.counters.writes).toBe(0);
    expect(unavailable.counters.random).toBe(0);
  });

  test("Windows DPAPI command keeps the key off argv and pins encryption, ACL, and atomic-write primitives", async () => {
    const calls: Array<AccountBridgeSecureCommand & { copiedInput: Buffer }> = [];
    let stored = "";
    const backend = createWindowsDPAPIBackend({
      targetPath: "C:\\Users\\test\\.crabcode\\account-bridge\\master-key.dpapi",
      powershellPath:
        "C:\\Windows\\System32\\WindowsPowerShell\\v1.0\\powershell.exe",
      async run(input) {
        const copiedInput = Buffer.from(input.input ?? []);
        calls.push({ ...input, copiedInput });
        const lines = copiedInput.toString("utf8").split("\n");
        if (lines[0] === "write") {
          stored = lines[2] ?? "";
          return { code: 0, stdout: "STORED" };
        }
        return stored === ""
          ? { code: 0, stdout: "ABSENT" }
          : {
              code: 0,
              stdout: `KEY:${Buffer.from(stored, "base64url").toString("base64")}`,
            };
      },
    });
    const deps: AccountBridgeMasterKeyDeps = {
      platform: "win32",
      randomBytes: size => new Uint8Array(size).fill(0x42),
      withLock: operation => operation(),
      createBackend: async () => backend,
    };
    const key = await loadOrCreateAccountBridgeMasterKey(deps);
    const encoded = Buffer.from(key).toString("base64url");
    expect(calls).toHaveLength(3);
    for (const call of calls) {
      expect(call.command).toBe(
        "C:\\Windows\\System32\\WindowsPowerShell\\v1.0\\powershell.exe",
      );
      expect(call.args.join(" ")).not.toContain(encoded);
      expect(call.args).toContain("-EncodedCommand");
    }
    expect(calls[1]!.copiedInput.toString("utf8")).toContain(encoded);
    expect(WINDOWS_DPAPI_SCRIPT).toContain("ProtectedData]::Protect");
    expect(WINDOWS_DPAPI_SCRIPT).toContain("DataProtectionScope]::CurrentUser");
    expect(WINDOWS_DPAPI_SCRIPT).toContain("Add-Type -AssemblyName System.Security");
    expect(WINDOWS_DPAPI_SCRIPT).toContain("SetAccessRuleProtection($true, $false)");
    expect(WINDOWS_DPAPI_SCRIPT).toContain("[IO.FileMode]::CreateNew");
    expect(WINDOWS_DPAPI_SCRIPT).toContain("$stream.Flush($true)");
    expect(WINDOWS_DPAPI_SCRIPT).toContain("[IO.File]::Move($temporary, $target)");
    expect(WINDOWS_DPAPI_SCRIPT).toContain("ReparsePoint");
    expect(WINDOWS_DPAPI_SCRIPT).toContain(".Replace('-', '+').Replace('_', '/')");
    expect(WINDOWS_DPAPI_SCRIPT).toContain("$standardBase64 += '='");
    expect(WINDOWS_DPAPI_SCRIPT).not.toContain("\\`n");
    expect(WINDOWS_DPAPI_SCRIPT).not.toContain("\\`r");
    for (const call of calls) call.copiedInput.fill(0);
  });

  test("Linux Secret Service uses fixed attributes and sends the key only on stdin", async () => {
    const calls: Array<AccountBridgeSecureCommand & { copiedInput: Buffer }> = [];
    let stored = "";
    const backend = createLinuxSecretServiceBackend({
      command: "/usr/bin/secret-tool",
      async run(input) {
        const copiedInput = Buffer.from(input.input ?? []);
        calls.push({ ...input, copiedInput });
        if (input.args[0] === "store") {
          stored = copiedInput.toString("utf8").trim();
          return { code: 0, stdout: "" };
        }
        return stored === ""
          ? { code: 1, stdout: "" }
          : { code: 0, stdout: `${stored}\n` };
      },
    });
    const deps: AccountBridgeMasterKeyDeps = {
      platform: "linux",
      randomBytes: size => new Uint8Array(size).fill(0x61),
      withLock: operation => operation(),
      createBackend: async () => backend,
    };
    const key = await loadOrCreateAccountBridgeMasterKey(deps);
    const encoded = Buffer.from(key).toString("base64url");
    expect(calls).toHaveLength(3);
    expect(calls[0]!.args).toEqual([
      "lookup",
      "application",
      "CrabCode",
      "service",
      "account-bridge-master-key",
      "schema-version",
      "1",
    ]);
    expect(calls[1]!.args[0]).toBe("store");
    expect(calls[1]!.args.join(" ")).not.toContain(encoded);
    expect(calls[1]!.copiedInput.toString("utf8").trim()).toBe(encoded);
    for (const call of calls) call.copiedInput.fill(0);
  });

  const windowsIntegrationTest = process.platform === "win32" ? test : test.skip;
  windowsIntegrationTest(
    "Windows stores only a DPAPI ciphertext and rejects a corrupted blob",
    async () => {
      const root = await fs.mkdtemp(join(tmpdir(), "ab-dpapi-"));
      const targetPath = join(root, "account-bridge", "master-key.dpapi");
      const systemRoot = process.env.SystemRoot ?? process.env.WINDIR;
      expect(systemRoot).toBeString();
      const powershellPath = join(
        systemRoot!,
        "System32",
        "WindowsPowerShell",
        "v1.0",
        "powershell.exe",
      );
      const backend = createWindowsDPAPIBackend({ targetPath, powershellPath });
      const deps: AccountBridgeMasterKeyDeps = {
        platform: "win32",
        randomBytes: size => new Uint8Array(size).fill(0x7a),
        withLock: operation => operation(),
        createBackend: async () => backend,
      };
      try {
        const first = await loadOrCreateAccountBridgeMasterKey(deps);
        const plaintext = Buffer.from(first);
        const encoded = plaintext.toString("base64url");
        const ciphertext = await fs.readFile(targetPath);
        expect(ciphertext.length).toBeGreaterThan(plaintext.length);
        expect(ciphertext.includes(plaintext)).toBeFalse();
        expect(ciphertext.toString("utf8")).not.toContain(encoded);
        const second = await loadOrCreateAccountBridgeMasterKey(deps);
        expect(Buffer.from(second)).toEqual(plaintext);

        await fs.writeFile(targetPath, Buffer.alloc(64, 0x5a));
        await expect(
          loadOrCreateAccountBridgeMasterKey(deps),
        ).rejects.toMatchObject({
          code: "master-key-secure-storage-unavailable",
        });
        plaintext.fill(0);
        ciphertext.fill(0);
      } finally {
        await fs.rm(root, { recursive: true, force: true });
      }
    },
    // Three sequential real powershell.exe DPAPI invocations; cold module
    // preparation alone can exceed bun's 5s default on a busy machine.
    30_000,
  );

  test("source has no generic/plaintext/fallback storage path", async () => {
    const source = await fs.readFile(
      join(
        import.meta.dir,
        "../../src/services/accountBridge/masterKey.ts",
      ),
      "utf8",
    );
    for (const forbidden of [
      "getSecureStorage",
      "plainTextStorage",
      "fallbackStorage",
      ".credentials.json",
    ]) {
      expect(source).not.toContain(forbidden);
    }
  });

  test("secure-storage unavailability is an explicit blocked runtime state", async () => {
    const fixture = fakeManagerDeps();
    fixture.deps.loadOrCreateMasterKey = async () => {
      throw new AccountBridgeMasterKeyError(
        "master-key-secure-storage-unavailable",
      );
    };
    const manager = new AccountBridgeManager(fixture.deps);
    await expect(manager.ensure()).rejects.toMatchObject({
      code: "master-key-secure-storage-unavailable",
    });
    expect(manager.status()).toMatchObject({
      state: "blocked",
      lastErrorCode: "master-key-secure-storage-unavailable",
    });
    expect(fixture.spawns).toHaveLength(0);
    expect(fixture.configs).toHaveLength(0);
  });
});

describe("Account Bridge login lifetime", () => {
  const zaiConnector = {
    connectorId: "zai",
    displayName: "Z Code",
    authMode: "device-code" as const,
    enabled: true,
    disabledReasonCode: null,
    termsStatus: "signed-off" as const,
  };

  test("Z.AI device-link login accepts a missing user_code", async () => {
    const fixture = fakeManagerDeps();
    const originalHTTP = fixture.deps.http;
    fixture.deps.http = async input => {
      if (input.url.includes("/connectors")) {
        return { connectors: [zaiConnector] };
      }
      if (input.url.includes("/login/start")) {
        return {
          status: "ok",
          url: "https://zcode.z.ai/authorize/abc",
          state: "zai-session-1",
          flow: "device",
          expires_in: 600,
        };
      }
      return originalHTTP(input);
    };
    const manager = new AccountBridgeManager(fixture.deps);
    await manager.ensure();
    await expect(manager.loginStart("zai")).resolves.toEqual({
      sessionId: "zai-session-1",
      authMode: "device-code",
      authorizationUrl: null,
      userCode: null,
      verificationUrl: "https://zcode.z.ai/authorize/abc",
      expiresAt: new Date(NOW + 600_000).toISOString(),
    });
    await manager.stop();
  });

  test("a present device user_code still receives strict validation", async () => {
    const fixture = fakeManagerDeps();
    const originalHTTP = fixture.deps.http;
    fixture.deps.http = async input => {
      if (input.url.includes("/connectors")) {
        return { connectors: [zaiConnector] };
      }
      if (input.url.includes("/login/start")) {
        return {
          status: "ok",
          url: "https://zcode.z.ai/authorize/abc",
          state: "zai-session-2",
          flow: "device",
          expires_in: 600,
          user_code: "",
        };
      }
      return originalHTTP(input);
    };
    const manager = new AccountBridgeManager(fixture.deps);
    await manager.ensure();
    await expect(manager.loginStart("zai")).rejects.toMatchObject({
      code: "login-start-response-invalid",
    });
    await manager.stop();
  });

  test("an untracked poll is terminal without calling the current sidecar", async () => {
    const fixture = fakeManagerDeps();
    const manager = new AccountBridgeManager(fixture.deps);
    await manager.ensure();
    const callsBefore = fixture.httpURLs.length;
    await expect(manager.loginPoll("untracked-session")).resolves.toEqual({
      state: "session-lost",
      accountId: null,
      errorCode: null,
    });
    expect(fixture.httpURLs).toHaveLength(callsBefore);
    await expect(manager.loginCancel("untracked-session")).resolves.toEqual({
      cancelled: true,
    });
    expect(fixture.httpURLs).toHaveLength(callsBefore);
    await manager.stop();
  });
});

describe("Account Bridge supply chain", () => {
  test("missing packaged paths retain the artifact-missing code", async () => {
    const root = await fs.mkdtemp(join(tmpdir(), "ab-artifact-missing-"));
    try {
      const binDir = join(root, "bin");
      await fs.mkdir(binDir);
      const binaryName =
        process.platform === "win32" ? "oauthapi-llm.exe" : "oauthapi-llm";
      await expect(
        verifyPackagedArtifact({
          binaryPath: join(binDir, binaryName),
          metadataDir: join(binDir, "account-bridge"),
          expectedComponentVersion: "1.0.13-account-bridge.1",
          expectedProtocolVersion: 1,
          expectedPlatform: "arm64-darwin",
          artifactPublicKeyBase64URL: Buffer.alloc(32, 1).toString("base64url"),
          eligibilityPublicKeyBase64URL: Buffer.alloc(32, 2).toString(
            "base64url",
          ),
        }),
      ).rejects.toMatchObject({ code: "artifact-missing" });
    } finally {
      await fs.rm(root, { recursive: true, force: true });
    }
  });

  test("requires packaged binary, lock/provenance SHA, component and protocol", async () => {
    const root = await fs.mkdtemp(join(process.env.TMPDIR ?? "/tmp", "ab-artifact-"));
    const binDir = join(root, "bin");
    const metadataDir = join(binDir, "account-bridge");
    await fs.mkdir(metadataDir, { recursive: true });
    const artifactSigner = generateKeyPairSync("ed25519");
    const artifactSpki = artifactSigner.publicKey.export({
      format: "der",
      type: "spki",
    });
    const artifactPublicKeyBytes = artifactSpki.subarray(artifactSpki.length - 32);
    const artifactPublicKey = artifactPublicKeyBytes.toString("base64url");
    const eligibilityPublicKeyBytes = Buffer.alloc(32, 0x25);
    const eligibilityPublicKey = eligibilityPublicKeyBytes.toString("base64url");
    const provenanceKeySha256 = createHash("sha256")
      .update(artifactPublicKeyBytes)
      .digest("hex");
    const pluginDir = join(metadataDir, "plugins");
    await fs.mkdir(pluginDir, { recursive: true });
    const binaryName = process.platform === "win32" ? "oauthapi-llm.exe" : "oauthapi-llm";
    const binaryPath = join(binDir, binaryName);
    const binary = Buffer.from("packaged-binary");
    await fs.writeFile(binaryPath, binary);
    const plugin = Buffer.from("packaged-gemini-plugin");
    const pluginPath = join(pluginDir, "gemini-cli.dylib");
    await fs.writeFile(pluginPath, plugin);
    const helper = Buffer.from("packaged-plugin-host-helper");
    const helperPath = join(binDir, "oauthapi-plugin-host");
    await fs.writeFile(helperPath, helper);
    const fixedPluginLicense = Buffer.from("MIT fixed plugin license");
    await fs.writeFile(
      join(pluginDir, "gemini-cli-LICENSE"),
      fixedPluginLicense,
    );
    const mainCodeDirectorySha256 = "c".repeat(64);
    const helperCodeDirectorySha256 = "d".repeat(64);
    const pluginCodeDirectorySha256 = "e".repeat(64);
    const sealEvidence = Buffer.from(
      JSON.stringify({
        schemaVersion: 1,
        scheme: "apple-ad-hoc",
        authenticity: "ed25519-provenance",
        provenanceKeySha256,
        notarization: "not-applicable",
        artifacts: [
          {
            path: `bin/${binaryName}`,
            sha256: createHash("sha256").update(binary).digest("hex"),
            codeDirectorySha256: mainCodeDirectorySha256,
          },
          {
            path: "bin/oauthapi-plugin-host",
            sha256: createHash("sha256").update(helper).digest("hex"),
            codeDirectorySha256: helperCodeDirectorySha256,
            entitlements: [
              "com.apple.security.cs.disable-library-validation",
            ],
          },
          {
            path: "plugins/gemini-cli.dylib",
            sha256: createHash("sha256").update(plugin).digest("hex"),
            codeDirectorySha256: pluginCodeDirectorySha256,
          },
        ],
      }),
    );
    await fs.writeFile(join(metadataDir, "codesign-evidence.json"), sealEvidence);
    const sealEvidenceSha256 = createHash("sha256")
      .update(sealEvidence)
      .digest("hex");
    const platformSignature = {
      scheme: "apple-ad-hoc",
      detachedSignature: null,
      provenanceKeySha256,
      codeDirectorySha256: mainCodeDirectorySha256,
      sealEvidenceSha256,
    };
    const runtimeHelpers = [
      {
        id: "plugin-host",
        path: "bin/oauthapi-plugin-host",
        sha256: createHash("sha256").update(helper).digest("hex"),
        platformSignature: {
          scheme: "apple-ad-hoc",
          detachedSignature: null,
          provenanceKeySha256,
          codeDirectorySha256: helperCodeDirectorySha256,
          sealEvidenceSha256,
        },
      },
    ];
    const lockedPluginTarget = {
      asset: "gemini-cli_1.0.5_darwin_arm64.zip",
      archiveSha256: "1".repeat(64),
      binary: "gemini-cli.dylib",
      binarySha256: "2".repeat(64),
    };
    const fixedPluginEvidence = {
      id: "gemini-cli",
      path: "plugins/gemini-cli.dylib",
      sha256: createHash("sha256").update(plugin).digest("hex"),
      source: {
        repository: "https://github.com/router-for-me/cpa-plugin-gemini-cli",
        release: "v1.0.5",
        commit: "f".repeat(40),
        asset: lockedPluginTarget.asset,
        archiveSha256: lockedPluginTarget.archiveSha256,
        binarySha256: lockedPluginTarget.binarySha256,
      },
      license: {
        path: "plugins/gemini-cli-LICENSE",
        sha256: createHash("sha256").update(fixedPluginLicense).digest("hex"),
      },
      platformSignature: {
        scheme: "apple-ad-hoc",
        detachedSignature: null,
        provenanceKeySha256,
        codeDirectorySha256: pluginCodeDirectorySha256,
        sealEvidenceSha256,
      },
    };
    const notice = Buffer.from("NOTICE");
    const thirdPartyNotices = Buffer.from("THIRD_PARTY_NOTICES.md");
    await fs.writeFile(join(metadataDir, "NOTICE"), notice);
    await fs.writeFile(
      join(metadataDir, "THIRD_PARTY_NOTICES.md"),
      thirdPartyNotices,
    );
    const licenseMaterialPath =
      "third-party-licenses/example.invalid/dependency/LICENSE";
    const licenseMaterial = Buffer.from("MIT dependency license");
    const licenseMaterialAbsolute = join(
      metadataDir,
      ...licenseMaterialPath.split("/"),
    );
    await fs.mkdir(dirname(licenseMaterialAbsolute), { recursive: true });
    await fs.writeFile(licenseMaterialAbsolute, licenseMaterial);
    const licenseMaterialsManifest = Buffer.from(
      `${JSON.stringify(
        {
          schemaVersion: 1,
          target: "darwin/arm64",
          scanner: {
            module: "github.com/google/go-licenses",
            version: "v1.6.0",
          },
          files: [
            {
              path: licenseMaterialPath,
              sha256: createHash("sha256")
                .update(licenseMaterial)
                .digest("hex"),
              size: licenseMaterial.length,
            },
          ],
        },
        null,
        2,
      )}\n`,
    );
    await fs.writeFile(
      join(metadataDir, "third-party-licenses.manifest.json"),
      licenseMaterialsManifest,
    );
    const license = Buffer.from("MIT test license");
    await fs.writeFile(join(metadataDir, "LICENSE"), license);
    const sbom = Buffer.from(
      JSON.stringify({
        bomFormat: "CycloneDX",
        metadata: {
          component: {
            name: "OAuthAPI-LLM",
            version: "1.0.13-account-bridge.1",
          },
        },
        components: [
          {
            type: "library",
            name: "cpa-plugin-gemini-cli",
            version: "1.0.5",
            purl: `pkg:github/router-for-me/cpa-plugin-gemini-cli@${"f".repeat(40)}`,
            hashes: [{ alg: "SHA-256", content: fixedPluginEvidence.sha256 }],
            licenses: [{ license: { id: "MIT" } }],
            externalReferences: [
              {
                type: "distribution",
                url: `https://github.com/router-for-me/cpa-plugin-gemini-cli/releases/download/v1.0.5/${lockedPluginTarget.asset}`,
                hashes: [
                  {
                    alg: "SHA-256",
                    content: lockedPluginTarget.archiveSha256,
                  },
                ],
              },
            ],
            properties: [
              {
                name: "crabcode:bundled-path",
                value: "plugins/gemini-cli.dylib",
              },
              {
                name: "crabcode:upstream-extracted-binary-sha256",
                value: lockedPluginTarget.binarySha256,
              },
              {
                name: "crabcode:license-sha256",
                value: fixedPluginEvidence.license.sha256,
              },
            ],
          },
        ],
      }),
    );
    await fs.writeFile(join(metadataDir, "sbom.cdx.json"), sbom);
    const lock = Buffer.from(
      JSON.stringify({
        schemaVersion: 1,
        component: "OAuthAPI-LLM",
        componentVersion: "1.0.13-account-bridge.1",
        protocolVersion: 1,
        module: "github.com/acosmi/OAuthAPI-LLM",
        upstream: {
          repository: "https://github.com/router-for-me/CLIProxyAPI",
          release: "v7.2.71",
          commit: "a".repeat(40),
          tree: "b".repeat(40),
          licenseSha256: createHash("sha256").update(license).digest("hex"),
        },
        fixedPlugins: {
          googleGeminiCli: {
            repository: "https://github.com/router-for-me/cpa-plugin-gemini-cli",
            release: "v1.0.5",
            commit: "f".repeat(40),
            commitSignature: {
              status: "verified",
              gpgKeyId: "5C3130A8F5AD576F",
            },
            license: "MIT",
            licensePath: "licenses/gemini-cli-LICENSE",
            licenseSha256: fixedPluginEvidence.license.sha256,
            distributionStatus: "fixed_release_assets_verified",
            targets: {
              "arm64-darwin": lockedPluginTarget,
              "x64-darwin": {
                asset: "gemini-cli_1.0.5_darwin_amd64.zip",
                archiveSha256: "3".repeat(64),
                binary: "gemini-cli.dylib",
                binarySha256: "4".repeat(64),
              },
              "arm64-linux": {
                asset: "gemini-cli_1.0.5_linux_arm64.zip",
                archiveSha256: "5".repeat(64),
                binary: "gemini-cli.so",
                binarySha256: "6".repeat(64),
              },
              "x64-linux": {
                asset: "gemini-cli_1.0.5_linux_amd64.zip",
                archiveSha256: "7".repeat(64),
                binary: "gemini-cli.so",
                binarySha256: "8".repeat(64),
              },
              "x64-win32": {
                asset: "gemini-cli_1.0.5_windows_amd64.zip",
                archiveSha256: "9".repeat(64),
                binary: "gemini-cli.dll",
                binarySha256: "a".repeat(64),
              },
            },
            requiredEvidence: [
              "release-asset-sha256",
              "extracted-binary-sha256",
              "license-sha256",
              "sbom",
              "provenance",
            ],
          },
        },
        importPolicy: {
          editableTruth: "components/oauthapi-llm",
          runtimeDownloadsAllowed: false,
          buildDownloadsRestrictedToLockedReleaseAssets: true,
          pathFallbackAllowed: false,
          nestedGitAllowed: false,
          mirrorDirection: "crabcode-to-acosmi-oauthapi-llm",
        },
      }),
    );
    await fs.writeFile(join(metadataDir, "UPSTREAM.lock"), lock);
    const provenance = {
      schemaVersion: 1,
      component: "OAuthAPI-LLM",
      componentVersion: "1.0.13-account-bridge.1",
      protocolVersion: 1,
      platform: "arm64-darwin",
      crabCodeCommit: "c".repeat(40),
      sourceTree: "d".repeat(40),
      upstreamLockSha256: createHash("sha256").update(lock).digest("hex"),
      materials: {
        fixedPluginLicense: {
          path: "plugins/gemini-cli-LICENSE",
          sha256: fixedPluginEvidence.license.sha256,
        },
        license: {
          path: "LICENSE",
          sha256: createHash("sha256").update(license).digest("hex"),
        },
        notice: {
          path: "NOTICE",
          sha256: createHash("sha256").update(notice).digest("hex"),
        },
        thirdPartyNotices: {
          path: "THIRD_PARTY_NOTICES.md",
          sha256: createHash("sha256").update(thirdPartyNotices).digest("hex"),
        },
        thirdPartyLicenseMaterials: {
          path: "third-party-licenses.manifest.json",
          sha256: createHash("sha256")
            .update(licenseMaterialsManifest)
            .digest("hex"),
        },
        upstreamLock: {
          path: "UPSTREAM.lock",
          sha256: createHash("sha256").update(lock).digest("hex"),
        },
        sbom: {
          path: "sbom.cdx.json",
          sha256: createHash("sha256").update(sbom).digest("hex"),
        },
        signatureEvidence: { path: "signature.json", sha256: "" },
      },
      sbom: {
        path: "sbom.cdx.json",
        sha256: createHash("sha256").update(sbom).digest("hex"),
      },
      binary: {
        path: `bin/${binaryName}`,
        sha256: createHash("sha256").update(binary).digest("hex"),
      },
      fixedPlugins: [fixedPluginEvidence],
      runtimeHelpers,
      platformSignature,
      signatureEvidenceSha256: "",
      build: {
        cgoEnabled: false,
        trimpath: true,
        buildvcs: false,
        eligibilityTrustRootSha256: createHash("sha256")
          .update(eligibilityPublicKeyBytes)
          .digest("hex"),
      },
    };
    const signatureEvidence = Buffer.from(
      JSON.stringify({
        schemaVersion: 1,
        component: "OAuthAPI-LLM",
        componentVersion: "1.0.13-account-bridge.1",
        platform: "arm64-darwin",
        binarySha256: provenance.binary.sha256,
        platformSignature: provenance.platformSignature,
        fixedPlugins: provenance.fixedPlugins,
        runtimeHelpers: provenance.runtimeHelpers,
      }),
    );
    await fs.writeFile(join(metadataDir, "signature.json"), signatureEvidence);
    provenance.signatureEvidenceSha256 = createHash("sha256")
      .update(signatureEvidence)
      .digest("hex");
    provenance.materials.signatureEvidence.sha256 =
      provenance.signatureEvidenceSha256;
    const writeSignedProvenance = async (): Promise<void> => {
      const raw = Buffer.from(JSON.stringify(provenance));
      await fs.writeFile(join(metadataDir, "provenance.json"), raw);
      await fs.writeFile(
        join(metadataDir, "provenance.sig"),
        sign(null, raw, artifactSigner.privateKey).toString("base64url"),
      );
    };
    await writeSignedProvenance();
    const verified = await verifyPackagedArtifact({
      binaryPath,
      metadataDir,
      expectedComponentVersion: "1.0.13-account-bridge.1",
      expectedProtocolVersion: 1,
      expectedPlatform: "arm64-darwin",
      artifactPublicKeyBase64URL: artifactPublicKey,
      eligibilityPublicKeyBase64URL: eligibilityPublicKey,
    });
    expect(verified).toMatchObject({
      componentVersion: "1.0.13-account-bridge.1",
      protocolVersion: 1,
      fixedPlugins: [
        {
          id: "gemini-cli",
          path: pluginPath,
          sha256: fixedPluginEvidence.sha256,
        },
      ],
    });

    await expect(
      verifyPackagedArtifact({
        binaryPath,
        metadataDir,
        expectedComponentVersion: "1.0.13-account-bridge.1",
        expectedProtocolVersion: 1,
        expectedPlatform: "arm64-darwin",
        artifactPublicKeyBase64URL: artifactPublicKey,
        eligibilityPublicKeyBase64URL: Buffer.alloc(32, 0x26).toString(
          "base64url",
        ),
      }),
    ).rejects.toMatchObject({ code: "provenance-invalid" });

    await fs.writeFile(pluginPath, "tampered plugin");
    await expect(
      verifyPackagedArtifact({
        binaryPath,
        metadataDir,
        expectedComponentVersion: "1.0.13-account-bridge.1",
        expectedProtocolVersion: 1,
        expectedPlatform: "arm64-darwin",
        artifactPublicKeyBase64URL: artifactPublicKey,
        eligibilityPublicKeyBase64URL: eligibilityPublicKey,
      }),
    ).rejects.toMatchObject({ code: "platform-signature-evidence-invalid" });
    await fs.writeFile(pluginPath, plugin);

    await fs.writeFile(helperPath, "tampered helper");
    await expect(
      verifyPackagedArtifact({
        binaryPath,
        metadataDir,
        expectedComponentVersion: "1.0.13-account-bridge.1",
        expectedProtocolVersion: 1,
        expectedPlatform: "arm64-darwin",
        artifactPublicKeyBase64URL: artifactPublicKey,
        eligibilityPublicKeyBase64URL: eligibilityPublicKey,
      }),
    ).rejects.toMatchObject({ code: "platform-signature-evidence-invalid" });
    await fs.writeFile(helperPath, helper);

    // Builder↔verifier drift tripwire: the builder always writes the
    // runtimeHelpers key into signature.json. A verifier that rejects it
    // bricked every v1.0.16 install (provenance-invalid on a byte-perfect
    // artifact); the legacy 7-key shape must stay rejected, never repaired.
    const legacyEvidence = Buffer.from(
      JSON.stringify({
        schemaVersion: 1,
        component: "OAuthAPI-LLM",
        componentVersion: "1.0.13-account-bridge.1",
        platform: "arm64-darwin",
        binarySha256: provenance.binary.sha256,
        platformSignature: provenance.platformSignature,
        fixedPlugins: provenance.fixedPlugins,
      }),
    );
    await fs.writeFile(join(metadataDir, "signature.json"), legacyEvidence);
    const signedEvidenceSha = provenance.signatureEvidenceSha256;
    provenance.signatureEvidenceSha256 = createHash("sha256")
      .update(legacyEvidence)
      .digest("hex");
    provenance.materials.signatureEvidence.sha256 =
      provenance.signatureEvidenceSha256;
    await writeSignedProvenance();
    await expect(
      verifyPackagedArtifact({
        binaryPath,
        metadataDir,
        expectedComponentVersion: "1.0.13-account-bridge.1",
        expectedProtocolVersion: 1,
        expectedPlatform: "arm64-darwin",
        artifactPublicKeyBase64URL: artifactPublicKey,
        eligibilityPublicKeyBase64URL: eligibilityPublicKey,
      }),
    ).rejects.toMatchObject({ code: "provenance-invalid" });
    await fs.writeFile(join(metadataDir, "signature.json"), signatureEvidence);
    provenance.signatureEvidenceSha256 = signedEvidenceSha;
    provenance.materials.signatureEvidence.sha256 = signedEvidenceSha;
    await writeSignedProvenance();

    provenance.protocolVersion = 2;
    await writeSignedProvenance();
    await expect(
      verifyPackagedArtifact({
        binaryPath,
        metadataDir,
        expectedComponentVersion: "1.0.13-account-bridge.1",
        expectedProtocolVersion: 1,
        expectedPlatform: "arm64-darwin",
        artifactPublicKeyBase64URL: artifactPublicKey,
        eligibilityPublicKeyBase64URL: eligibilityPublicKey,
      }),
    ).rejects.toMatchObject({ code: "provenance-invalid" });

    provenance.protocolVersion = 1;
    await writeSignedProvenance();
    await fs.writeFile(join(metadataDir, "NOTICE"), "tampered notice");
    await expect(
      verifyPackagedArtifact({
        binaryPath,
        metadataDir,
        expectedComponentVersion: "1.0.13-account-bridge.1",
        expectedProtocolVersion: 1,
        expectedPlatform: "arm64-darwin",
        artifactPublicKeyBase64URL: artifactPublicKey,
        eligibilityPublicKeyBase64URL: eligibilityPublicKey,
      }),
    ).rejects.toMatchObject({ code: "provenance-invalid" });
    await fs.rm(root, { recursive: true, force: true });
  });

  test("stdio semantics and minimal child environment are fixed per platform", () => {
    expect(accountBridgeStdio("darwin")).toEqual(["ignore", "pipe", "pipe", "pipe"]);
    expect(accountBridgeStdio("win32")).toEqual(["pipe", "pipe", "pipe"]);
    const env = sanitizedSidecarEnv({
      PATH: "/safe",
      HTTP_PROXY: "http://secret",
      https_proxy: "http://secret",
      ALL_PROXY: "socks://secret",
      no_proxy: "localhost",
      HOME_JWT: "remote-control-plane-secret",
      MANAGEMENT_PASSWORD: "second-management-key",
      GITSTORE_GIT_TOKEN: "store-secret",
      CRABCODE_CONFIG_DIR: "/must-not-cross-process-boundary",
      DYLD_INSERT_LIBRARIES: "/tmp/inject.dylib",
      TMPDIR: "/safe-tmp",
      LANG: "en_US.UTF-8",
    });
    expect(env).toEqual({
      PATH: "/safe",
      TMPDIR: "/safe-tmp",
      LANG: "en_US.UTF-8",
    });
  });

  test("accepts only the exact machine-readable readiness line", () => {
    expect(
      parseAccountBridgeReadyLine(
        '{"event":"account-bridge-ready","protocolVersion":1,"address":"127.0.0.1","port":43123}',
        1,
      ),
    ).toBe("http://127.0.0.1:43123");
    expect(
      parseAccountBridgeReadyLine(
        "API server started successfully on: 127.0.0.1:43123",
        1,
      ),
    ).toBeNull();
    expect(() =>
      parseAccountBridgeReadyLine(
        '{"event":"account-bridge-ready","protocolVersion":2,"address":"127.0.0.1","port":43123}',
        1,
      ),
    ).toThrow("runtime-readiness-invalid");
  });

  test("owns the full sidecar process tree on every supported platform", () => {
    expect(
      accountBridgeSpawnCommand(
        "/bundle/oauthapi-llm",
        "/private/config.json",
        "darwin",
        {},
      ),
    ).toEqual({
      file: "/bundle/oauthapi-llm",
      args: ["-config", "/private/config.json"],
      detached: true,
    });
    expect(
      accountBridgeSpawnCommand(
        "C:\\bundle\\oauthapi-llm.exe",
        "C:\\private\\config.json",
        "win32",
        { CRABCODE_PROCESS_TREE_EXECUTABLE: "C:\\bundle\\crabcode.exe" },
      ),
    ).toEqual({
      file: "C:\\bundle\\crabcode.exe",
      args: [
        "process-tree-exec",
        "--",
        "C:\\bundle\\oauthapi-llm.exe",
        "-config",
        "C:\\private\\config.json",
      ],
      detached: false,
    });
    expect(() =>
      accountBridgeSpawnCommand(
        "C:\\bundle\\oauthapi-llm.exe",
        "C:\\private\\config.json",
        "win32",
        {},
      ),
    ).toThrow("runtime-process-tree-helper-unavailable");
  });
});

describe("Account Bridge direct-runtime and default-model parity", () => {
  test("exposes direct turn access without a host-method registry", () => {
    expect(acquireDirectAccountBridgeTurnAccess).toBeFunction();
  });

  test("runtime/status refreshes local diagnostics without widening its DTO", async () => {
    const fixture = fakeManagerDeps({ enabled: true });
    const manager = new AccountBridgeManager(fixture.deps);
    await manager.ensure();
    await manager.refreshLocalDiagnostics();
    const result = manager.status();
    expect(result).toEqual({
      state: "ready",
      componentVersion: "1.0.13-account-bridge.1",
      protocolVersion: 1,
      lastErrorCode: null,
    });
    expect(fixture.diagnosticBundles).toHaveLength(1);
    expect(Object.keys(result as Record<string, unknown>).sort()).toEqual([
      "componentVersion",
      "lastErrorCode",
      "protocolVersion",
      "state",
    ]);
    await manager.stop();
  });

  test("mints turn access for a settings-resolved account model without gateway fallback", async () => {
    let requestedRoute = "";
    const access = await acquireDirectAccountBridgeTurnAccess(undefined, {
      resolveDefaultModel: async () => `account:${ROUTE_A}`,
      turnAccess: async routeId => {
        requestedRoute = routeId;
        return {
          endpoint: "http://127.0.0.1:43123/v1/messages",
          route: route(),
          inferenceKey: "C".repeat(43),
        };
      },
    });
    expect(requestedRoute).toBe(ROUTE_A);
    expect(access.modelForQuery).toBe(`account:${ROUTE_A}`);
    expect(access.runtimeAccess?.route.routeId).toBe(ROUTE_A);
    access.release();
  });

  test("rejects an access capability for another route", async () => {
    await expect(
      acquireDirectAccountBridgeTurnAccess(`account:${ROUTE_A}`, {
        resolveDefaultModel: async () => "unused",
        turnAccess: async () => ({
          endpoint: "http://127.0.0.1:43123/v1/messages",
          route: route(ROUTE_B),
          inferenceKey: "C".repeat(43),
        }),
      }),
    ).rejects.toMatchObject({
      code: "account-route-access-mismatch",
    } satisfies Partial<DirectAccountBridgeTurnError>);
  });
});
