import { createHash } from "node:crypto";
import { readFile } from "node:fs/promises";
import { join } from "node:path";
import { describe, expect, test } from "bun:test";
import {
  accountBridgeReleasePins,
  publicAccountBridgePlatforms,
} from "../../scripts/release-account-bridge-pins.mjs";

const root = join(import.meta.dir, "../..");

describe("release Account Bridge pins", () => {
  test("bind the repository lock and exact public platform asset set", async () => {
    const lockRaw = await readFile(
      join(root, "components/oauthapi-llm/UPSTREAM.lock"),
    );
    const lock = JSON.parse(lockRaw.toString("utf8"));
    expect(createHash("sha256").update(lockRaw).digest("hex")).toBe(
      accountBridgeReleasePins.upstreamLockSha256,
    );
    expect(lock.componentVersion).toBe(
      accountBridgeReleasePins.componentVersion,
    );
    expect(lock.protocolVersion).toBe(accountBridgeReleasePins.protocolVersion);
    expect(publicAccountBridgePlatforms).toEqual([
      "arm64-darwin",
      "x64-darwin",
      "x64-win32",
    ]);
    for (const pin of Object.values(accountBridgeReleasePins.platforms)) {
      expect(pin.asset).toMatch(/^oauthapi-llm-[a-z0-9-]+\.zip$/);
      expect(pin.sha256).toMatch(/^[a-f0-9]{64}$/);
    }
  });

  test("runs the signed-artifact gate before the hosted native build", async () => {
    const releaseWorkflow = await readFile(
      join(root, ".github/workflows/release.yml"),
      "utf8",
    );
    const ciWorkflow = await readFile(
      join(root, ".github/workflows/ci.yml"),
      "utf8",
    );
    const gate = "bun scripts/verify-release-account-bridge.mjs";
    const hostedBuild = "  build-windows:";
    expect(releaseWorkflow).toContain(
      `ACCOUNT_BRIDGE_ARTIFACT_PUBLIC_KEY_BASE64URL: ${accountBridgeReleasePins.artifactPublicKeyBase64URL}`,
    );
    expect(ciWorkflow).toContain(
      `ACCOUNT_BRIDGE_ARTIFACT_PUBLIC_KEY_BASE64URL: ${accountBridgeReleasePins.artifactPublicKeyBase64URL}`,
    );
    expect(releaseWorkflow.indexOf(gate)).toBeGreaterThan(-1);
    expect(releaseWorkflow.indexOf(hostedBuild)).toBeGreaterThan(-1);
    expect(releaseWorkflow.indexOf(gate)).toBeLessThan(
      releaseWorkflow.indexOf(hostedBuild),
    );
  });
});
