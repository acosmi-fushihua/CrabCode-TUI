import { readFile } from "node:fs/promises";
import { join } from "node:path";
import { describe, expect, test } from "bun:test";

const root = join(import.meta.dir, "../..");

describe("Windows release recovery quota controls", () => {
  test("writes native generation markers with protocol LF on Windows", async () => {
    const installer = await readFile(join(root, "scripts/install.ps1"), "utf8");

    expect(installer).not.toContain("[Environment]::NewLine");
    expect(installer.match(/\+ "`n"/g)).toHaveLength(2);
    expect(installer).toContain("native generation-marker protocol");
  });

  test("makes PE linking deterministic before the hosted native build", async () => {
    const workflow = await readFile(
      join(root, ".github/workflows/release.yml"),
      "utf8",
    );
    const probe = "Prove deterministic Windows PE linking before native allocation";
    const nativeBuild = "\n      - name: Build native Rust TUI product closure";

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

  test("can reuse a preserved candidate without allocating a Rust build", async () => {
    const workflow = await readFile(
      join(root, ".github/workflows/release.yml"),
      "utf8",
    );
    const preserved = "inputs.recovery_strategy == 'preserved-artifact'";
    const noPreservedBuild =
      "github.event_name != 'workflow_dispatch' || inputs.recovery_strategy != 'preserved-artifact'";

    expect(workflow).toContain("candidate_run_id:");
    expect(workflow).toContain("candidate_windows_sha256:");
    expect(workflow).toContain("actions/download-artifact@37930b1c2abaa49bbe596cd826c3c89aef350131");
    expect(workflow).toContain("Bind the preserved Windows candidate bytes");
    expect(workflow).toContain("Verify preserved Windows candidate provenance");
    expect(
      workflow.match(
        new RegExp(
          noPreservedBuild.replace(/[.*+?^${}()|[\]\\]/g, "\\$&"),
          "g",
        ),
      ),
    ).toHaveLength(7);
    expect(workflow).toContain(preserved);
    expect(workflow).toContain(
      'git restore --source="${RELEASE_TOOLING_SHA}" --staged --worktree -- \\',
    );
    expect(workflow).toContain(
      'git diff --cached --quiet "${RELEASE_TOOLING_SHA}" -- \\',
    );
    expect(workflow).toContain("git diff --quiet -- \\");
    expect(workflow).toContain("scripts/install.ps1 \\");
    expect(workflow).toContain("scripts/release-package-smoke.mjs");
    expect(workflow).toContain("tests/windows/install-bootstrap-transport.ps1");
  });
});
