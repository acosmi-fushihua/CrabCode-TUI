import { describe, expect, test } from "bun:test";
import type { ChildProcess } from "node:child_process";
import { EventEmitter } from "node:events";
import { PassThrough } from "node:stream";
import {
  childProcessAdapter,
  FACADE_ERROR_BODY_LIMIT_BYTES,
  parseSidecarLogLine,
  readBoundedFacadeErrorDetail,
  redactSidecarLogMessage,
  SIDECAR_FORENSIC_LINE_CAP,
} from "../../src/services/accountBridge/runtimeManager.js";

const BANNER =
  "OAuthAPI-LLM Version: 7.2.71-crabcode.6, Commit: d2d39f4da4c83b0c2d4d6eea52caad55a2ca63fa, BuiltAt: 1970-01-01T00:00:00.000Z";
const FATAL =
  "[2026-08-03 06:02:29] [--------] [error] [main.go:171] Account Bridge bootstrap rejected";

function fakeChild(): {
  child: ChildProcess;
  stdout: PassThrough;
  stderr: PassThrough;
  exit(code: number | null, signal: NodeJS.Signals | null): void;
  killCount(): number;
} {
  const emitter = new EventEmitter() as unknown as ChildProcess & {
    stdout: PassThrough;
    stderr: PassThrough;
  };
  const stdout = new PassThrough();
  const stderr = new PassThrough();
  emitter.stdout = stdout;
  emitter.stderr = stderr;
  (emitter as unknown as { pid: number }).pid = 4242;
  let kills = 0;
  (emitter as unknown as { kill: () => boolean }).kill = () => {
    kills += 1;
    return true;
  };
  return {
    child: emitter,
    stdout,
    stderr,
    exit: (code, signal) => emitter.emit("exit", code, signal),
    killCount: () => kills,
  };
}

const settle = (): Promise<void> =>
  new Promise(resolve => setTimeout(resolve, 5));

describe("Account Bridge sidecar forensics", () => {
  test("retains a bounded branch anchor and redacts paths", () => {
    expect(parseSidecarLogLine(FATAL, "stdout")).toEqual({
      stream: "stdout",
      timestamp: "2026-08-03 06:02:29",
      level: "error",
      origin: "main.go:171",
      message: "Account Bridge bootstrap rejected",
      redactedByteLength: null,
    });
    const redacted = redactSidecarLogMessage(
      "failed to resolve auth directory: /Users/example/.crabcode/account-bridge/auth",
    );
    expect(redacted.message).toBe("failed to resolve auth directory");

    const spacedPath = redactSidecarLogMessage(
      "failed to open /Users/Alice/My File/token.json: permission denied",
    );
    expect(spacedPath.message).toBe("sidecar operation failed");
    expect(spacedPath.message).not.toContain("Alice");
    expect(spacedPath.message).not.toContain("File/token.json");

    const windowsSpacedPath = redactSidecarLogMessage(
      "failed to open C:\\Users\\Alice\\My File\\token.json",
    );
    expect(windowsSpacedPath.message).toBe("sidecar operation failed");

    const shortEmailBasename = redactSidecarLogMessage(
      "failed to parse auth file a@b.co: invalid JSON",
    );
    expect(shortEmailBasename.message).toBe("failed to parse auth file");
    expect(shortEmailBasename.message).not.toContain("a@b.co");

    const pluginhostAuthId = redactSidecarLogMessage(
      "pluginhost: models for auth a@b.co failed",
    );
    expect(pluginhostAuthId.message).toBe("pluginhost operation failed");
    expect(pluginhostAuthId.message).not.toContain("a@b.co");

    const bootstrapAuthId = redactSidecarLogMessage(
      "Account Bridge bootstrap rejected for auth a@b.co",
    );
    expect(bootstrapAuthId.message).toBe(
      "Account Bridge bootstrap rejected",
    );
    expect(bootstrapAuthId.message).not.toContain("a@b.co");
  });

  test("records exit evidence and caps a chatty sidecar", async () => {
    const fake = fakeChild();
    const adapter = childProcessAdapter(fake.child, "linux", 1);
    const endpoint = adapter.endpoint.catch(
      error => (error as { code?: string }).code,
    );
    fake.stdout.write(`${BANNER}\n`);
    for (let index = 0; index < SIDECAR_FORENSIC_LINE_CAP + 3; index++) {
      fake.stderr.write(`${FATAL}\n`);
    }
    await settle();
    fake.exit(1, null);
    expect(await endpoint).toBe("runtime-exited-before-ready");
    const forensics = adapter.forensics();
    expect(forensics.exitCode).toBe(1);
    expect(forensics.bannerLine).toBe(BANNER);
    expect(forensics.logLines).toHaveLength(SIDECAR_FORENSIC_LINE_CAP);
    expect(forensics.droppedLineCount).toBe(3);
  });

  test("a rejected readiness line keeps its real failure code", async () => {
    const fake = fakeChild();
    const adapter = childProcessAdapter(fake.child, "linux", 1);
    fake.stdout.write(
      `${JSON.stringify({
        event: "account-bridge-ready",
        protocolVersion: 99,
        address: "127.0.0.1",
        port: 43123,
      })}\n`,
    );
    await expect(adapter.endpoint).rejects.toMatchObject({
      code: "runtime-readiness-invalid",
    });
    expect(adapter.forensics().readyLineRejected).toBeTrue();
    expect(fake.killCount()).toBe(1);
  });
});

describe("bounded facade error details", () => {
  test("keeps a stable callback failure code", async () => {
    await expect(
      readBoundedFacadeErrorDetail(
        Response.json({ error: "callback_port_busy" }, { status: 500 }),
      ),
    ).resolves.toBe("callback_port_busy");
  });

  test("drops oversized or free-text bodies", async () => {
    const oversized = JSON.stringify({
      error: "x".repeat(FACADE_ERROR_BODY_LIMIT_BYTES + 100),
    });
    await expect(
      readBoundedFacadeErrorDetail(
        new Response(oversized, { status: 500 }),
      ),
    ).resolves.toBeNull();
    await expect(
      readBoundedFacadeErrorDetail(
        Response.json(
          { error: "failed to start callback server" },
          { status: 500 },
        ),
      ),
    ).resolves.toBeNull();
  });
});
