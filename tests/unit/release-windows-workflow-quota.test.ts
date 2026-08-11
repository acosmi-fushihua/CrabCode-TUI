import { readFile } from "node:fs/promises";
import { join } from "node:path";
import { describe, expect, test } from "bun:test";

const root = join(import.meta.dir, "../..");

describe("Windows release recovery quota controls", () => {
  test("makes PE linking deterministic before the hosted native build", async () => {
    const workflow = await readFile(
      join(root, ".github/workflows/release.yml"),
      "utf8",
    );
    const probe = "Prove deterministic Windows PE linking before native allocation";
    const nativeBuild = "Build native Rust TUI product closure";

    expect(workflow).toContain(
      "RUSTFLAGS: -C link-arg=/Brepro -C link-arg=/DEBUG:NONE",
    );
    expect(workflow).toContain("'strip=symbols'");
    expect(workflow).toContain("'link-arg=/Brepro'");
    expect(workflow).toContain("'link-arg=/DEBUG:NONE'");
    expect(workflow.indexOf(probe)).toBeGreaterThan(-1);
    expect(workflow.indexOf(nativeBuild)).toBeGreaterThan(-1);
    expect(workflow.indexOf(probe)).toBeLessThan(workflow.indexOf(nativeBuild));
  });

  test("preserves the candidate before evidence and smoke assertions", async () => {
    const workflow = await readFile(
      join(root, ".github/workflows/release.yml"),
      "utf8",
    );
    const upload = "actions/upload-artifact@043fb46d1a93c77aae656e7c1c64a875d1fc6a0a";
    const exactByteGate = "Bind exact-byte recovery to the previously replayed package";
    const recoverySmoke = "Complete installer-first recovery and one direct confirmation replay";

    expect(workflow.indexOf(upload)).toBeGreaterThan(-1);
    expect(workflow.indexOf(exactByteGate)).toBeGreaterThan(-1);
    expect(workflow.indexOf(recoverySmoke)).toBeGreaterThan(-1);
    expect(workflow.indexOf(upload)).toBeLessThan(workflow.indexOf(exactByteGate));
    expect(workflow.indexOf(upload)).toBeLessThan(workflow.indexOf(recoverySmoke));
    expect(workflow).toContain("inputs.recovery_strategy == 'exact-bytes'");
  });

  test("reuses prior 100-replay evidence without repeating it during recovery", async () => {
    const workflow = await readFile(
      join(root, ".github/workflows/release.yml"),
      "utf8",
    );
    const repeatedGate = workflow.indexOf("Replay the incident 100 times from the assembled package");
    const recoveryGate = workflow.indexOf(
      "Complete installer-first recovery and one direct confirmation replay",
    );
    const repeatedSlice = workflow.slice(repeatedGate, recoveryGate);
    const recoverySlice = workflow.slice(recoveryGate);

    expect(repeatedSlice).toContain("if: github.event_name == 'push'");
    expect(repeatedSlice).toContain("--iterations 100");
    expect(recoverySlice).toContain("if: github.event_name == 'workflow_dispatch'");
    expect(recoverySlice).toContain("--iterations 1");
  });
});
