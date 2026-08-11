import { readFile } from "node:fs/promises";
import { join } from "node:path";
import { describe, expect, test } from "bun:test";
import {
  bunReleaseForPlatform,
  defaultBunRelease,
} from "../../scripts/release-bun-pins.mjs";

const root = join(import.meta.dir, "../..");

describe("packaged Bun release pins", () => {
  test("keep every public package on the runtime that parses the bundled graph", async () => {
    expect(defaultBunRelease.version).toBe("1.3.14");
    for (const platform of ["arm64-darwin", "x64-darwin", "x64-win32"]) {
      expect(bunReleaseForPlatform(platform).version).toBe("1.3.14");
    }

    for (const workflow of ["ci.yml", "release.yml"]) {
      const contents = await readFile(
        join(root, ".github/workflows", workflow),
        "utf8",
      );
      expect(contents).toContain("BUN_VERSION: '1.3.14'");
      expect(contents).not.toContain("BUN_VERSION: '1.3.11'");
    }
  });
});
