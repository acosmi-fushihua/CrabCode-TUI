import { readFile } from "node:fs/promises";
import { join } from "node:path";
import { describe, expect, test } from "bun:test";

const root = join(import.meta.dir, "../..");

describe("public release installer bootstraps", () => {
  test("keeps the Windows irm bootstrap printable ASCII without a BOM", async () => {
    const bytes = await readFile(join(root, "scripts/install.ps1"));
    const allowed = (byte: number) =>
      byte === 0x09 ||
      byte === 0x0a ||
      byte === 0x0d ||
      (byte >= 0x20 && byte <= 0x7e);

    expect(bytes.length).toBeGreaterThan(0);
    expect(bytes[0]).toBe(0x23);
    expect([...bytes.subarray(0, 3)]).not.toEqual([0xef, 0xbb, 0xbf]);
    expect([...bytes].every(allowed)).toBe(true);

    const installer = bytes.toString("ascii");
    expect(installer).not.toContain("api.github.com");
    expect(installer).toContain("Get-ReleaseRecordFromChecksum");
    expect(installer).toContain(
      "https://github.com/$Repository/releases/latest/download/checksums-sha256.txt",
    );
  });

  test("discovers the macOS latest version without api.github.com", async () => {
    const installer = await readFile(
      join(root, "scripts/install.sh"),
      "utf8",
    );

    expect(installer).not.toContain("api.github.com");
    expect(installer).toContain("LATEST_ARCHIVES");
    expect(installer).toContain(
      "releases/latest/download/checksums-sha256.txt",
    );
  });

  test("runs the real Windows wire bootstrap in both supported PowerShell engines", async () => {
    const workflow = await readFile(
      join(root, ".github/workflows/release.yml"),
      "utf8",
    );
    const transport = await readFile(
      join(root, "tests/windows/install-bootstrap-transport.ps1"),
      "utf8",
    );

    expect(workflow).toContain(
      "Exercise irm pipe install on Windows PowerShell 5.1",
    );
    expect(workflow).toContain(
      "Exercise irm pipe install on PowerShell 7",
    );
    expect(
      workflow.match(
        /run: \.\/tests\/windows\/install-bootstrap-transport\.ps1 -InstallerPath scripts\/install\.ps1/g,
      ),
    ).toHaveLength(2);
    expect(transport).toContain("Content-Type: application/octet-stream");
    expect(transport).toContain("irm $url | iex");
    expect(transport).toContain("CommandNotFoundException");
    expect(transport).toContain("Get-ReleaseRecordFromChecksum");
    expect(transport).toContain("$ast.FindAll");
    expect(transport).toContain(
      "TcpListener]::new([Net.IPAddress]::Loopback, 0)",
    );
    expect(transport).toContain(
      "[IO.File]::Move($readyTemporary, $Ready)",
    );
    expect(transport).toContain("if ($hadVersion)");
    expect(transport).toContain("if ($hadAssetDirectory)");
    expect(transport).toContain("$unrelatedLine");
    expect(transport).toContain("$tabLine");
  });

  test("runs the real macOS curl pipe bootstrap through release-like transport", async () => {
    const workflow = await readFile(
      join(root, ".github/workflows/ci.yml"),
      "utf8",
    );
    const transport = await readFile(
      join(root, "tests/macos/install-bootstrap-transport.sh"),
      "utf8",
    );
    const server = await readFile(
      join(root, "tests/fixtures/bootstrap-http-server.mjs"),
      "utf8",
    );

    expect(workflow).toContain(
      "Exercise curl pipe install through Release transport",
    );
    expect(workflow).toContain("endsWith(matrix.platform, '-darwin')");
    expect(transport).toContain(
      'OUTPUT="$(curl -fsSL "${BASE_URL}/latest/download/install.sh" | sh 2>&1)"',
    );
    expect(transport).toContain('export PATH="${SHIM_DIR}:${ORIGINAL_PATH}"');
    expect(transport).toContain('"${REAL_CURL}" "$@"');
    expect(transport).toContain(
      "releases/latest/download/checksums-sha256.txt",
    );
    expect(transport).toContain(
      "releases/download/v${VERSION}/${ARCHIVE}",
    );
    expect(server).toContain("application/octet-stream");
    expect(server).toContain("statusCode = 302");
  });
});
